use super::{Access, Runtime};
use crate::{model::*, scheduling::JobState};
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use tokio::sync::{OwnedRwLockReadGuard, RwLock, watch};

#[derive(Clone, Debug)]
pub struct ManagerAccess {
    pub attempt_id: String,
    pub trigger: String,
    pub target_node: String,
    pub read_only: bool,
    pub followup_version: Option<u64>,
    pub permission_version: u64,
    pub session: AgentSession,
}

pub(super) struct ActiveManager {
    manager: ManagerAccess,
    network_id: String,
    conversation_id: String,
    token: String,
    cancel: watch::Sender<bool>,
    fenced: AtomicBool,
    admission: Arc<RwLock<()>>,
}

impl Runtime {
    pub async fn register_manager_attempt(
        &self,
        token: String,
        session: AgentSession,
        attempt_id: &str,
        trigger: &str,
        read_only: bool,
    ) -> Result<watch::Receiver<bool>> {
        if let Some(active) = self.manager_controls.lock().await.get(attempt_id) {
            return Ok(active.cancel.subscribe());
        }
        let target = self.identity.node_id()?;
        let scope = session.conversation_id.as_str();
        let job = self
            .store
            .lock()
            .await
            .scheduler_job(session.space_id.as_str(), scope, trigger, &target)?
            .context("manager job missing")?;
        let attempt = job.state.attempt().context("manager attempt missing")?;
        ensure!(
            attempt.id == attempt_id && attempt.read_only == read_only,
            "manager attempt mismatch"
        );
        let manager = ManagerAccess {
            attempt_id: attempt_id.into(),
            trigger: trigger.into(),
            target_node: target,
            read_only,
            followup_version: attempt.followup_version,
            permission_version: attempt.permission_version,
            session: session.clone(),
        };
        let (cancel, receiver) = watch::channel(false);
        let active = Arc::new(ActiveManager {
            manager,
            network_id: session.space_id.to_string(),
            conversation_id: session.conversation_id.clone(),
            token: token.clone(),
            cancel,
            fenced: AtomicBool::new(false),
            admission: Arc::new(RwLock::new(())),
        });
        self.agent_tokens.lock().await.insert(token, session);
        let mut controls = self.manager_controls.lock().await;
        if let Some(existing) = controls.get(attempt_id) {
            ensure!(
                existing.manager.session.sdk_session == active.manager.session.sdk_session,
                "management attempt was reused for another SDK session"
            );
            return Ok(existing.cancel.subscribe());
        }
        controls.insert(attempt_id.into(), active);
        Ok(receiver)
    }

    pub async fn fence_manager_attempt(&self, attempt_id: &str) -> Result<()> {
        let active = self.manager_controls.lock().await.get(attempt_id).cloned();
        if let Some(active) = active {
            active.fenced.store(true, Ordering::Release);
            self.agent_tokens.lock().await.remove(&active.token);
            let _ = active.cancel.send(true);
            let manager = &active.manager;
            self.store.lock().await.fence_manager_permission(
                &active.network_id,
                &active.conversation_id,
                &manager.target_node,
                &manager.trigger,
                &manager.attempt_id,
            )?;
            // Wait for admitted tools before declaring this attempt unable to write.
            let _drained = active.admission.clone().write_owned().await;
        }
        Ok(())
    }

    pub async fn finish_manager_attempt(&self, attempt_id: &str) {
        let active = self.manager_controls.lock().await.get(attempt_id).cloned();
        if let Some(active) = active {
            active.fenced.store(true, Ordering::Release);
            self.agent_tokens.lock().await.remove(&active.token);
            let _drained = active.admission.clone().write_owned().await;
            self.manager_controls.lock().await.remove(attempt_id);
        }
    }

    pub(super) async fn manager_tool_lease(
        &self,
        access: &Access,
        write: bool,
        task: Option<&str>,
    ) -> Result<Option<OwnedRwLockReadGuard<()>>> {
        let Some(manager) = &access.manager else {
            return Ok(None);
        };
        let active = self
            .manager_controls
            .lock()
            .await
            .get(&manager.attempt_id)
            .cloned()
            .context("management invocation is no longer active")?;
        let guard = active.admission.clone().read_owned().await;
        ensure!(
            !active.fenced.load(Ordering::Acquire),
            "management invocation has been stopped"
        );
        ensure!(
            active.network_id == access.network_id
                && access.conversation_id.as_deref() == Some(active.conversation_id.as_str())
                && active.manager.session.sdk_session == manager.session.sdk_session
                && active.manager.permission_version == manager.permission_version,
            "management invocation scope mismatch"
        );
        ensure!(
            !write || !manager.read_only,
            "this management invocation is read-only"
        );
        Self::check_manager_permission(&*self.store.lock().await, access, write, task)?;
        Ok(Some(guard))
    }

    pub(super) fn check_manager_permission(
        store: &crate::store::Store,
        access: &Access,
        write: bool,
        task: Option<&str>,
    ) -> Result<()> {
        let Some(manager) = &access.manager else {
            return Ok(());
        };
        let scope = access
            .conversation_id
            .as_deref()
            .context("missing management scope")?;
        ensure!(
            store.manager_attempt_allowed(
                &access.network_id,
                scope,
                &manager.target_node,
                &manager.trigger,
                &manager.attempt_id,
                write
            )?,
            "management invocation permission is no longer valid"
        );
        if let Some(version) = manager.followup_version {
            if write {
                ensure!(
                    task.is_some(),
                    "autonomous follow-up cannot create unrelated work"
                );
            }
            store.validate_latest_manager_policy(
                &access.network_id,
                scope,
                &manager.trigger,
                &manager.target_node,
                version,
                if write { task } else { None },
            )?;
        }
        Ok(())
    }

    async fn manager_target(&self, network: &str, scope: &str, trigger: &str) -> Result<String> {
        let local = self.identity.node_id()?;
        let store = self.store.lock().await;
        if store
            .scheduler_job(network, scope, trigger, &local)?
            .is_some()
        {
            return Ok(local);
        }
        let context = store.context(network, scope, &[])?;
        let event = context
            .events
            .iter()
            .find(|event| {
                crate::crypto::digest(&serde_json::to_vec(event).unwrap_or_default()) == trigger
            })
            .context("management trigger not found")?;
        ensure!(event.scope_id == scope, "management trigger scope mismatch");
        match &event.body {
            EventBody::MessagePosted {
                role,
                scheduler_node: Some(target),
                ..
            } if role == "user" => Ok(target.clone()),
            EventBody::ExecutionObserved {
                task_id: Some(task),
                ..
            } => Ok(store.followup_policy(network, scope, task)?.target_node),
            EventBody::ExecutionObserved { task_id: None, .. } => Ok(store
                .execution_result_policy(network, scope, trigger)?
                .target_node),
            _ => anyhow::bail!("event is not a management trigger"),
        }
    }

    async fn forward_manager_control(&self, access: &Access, target: &str) -> Result<Value> {
        let key = self
            .store
            .lock()
            .await
            .authorize_node(&access.network_id, target, None)?
            .public_key;
        let device = vibeface_secure::authorization::DeviceId::from_key(
            &vibeface_secure::authorization::PublicKey::parse(&key)?,
        );
        ensure!(
            access.authorization.audience().device == device,
            "prepare the manager control for its scheduling Node before signing"
        );
        self.peer_request(
            &access.network_id,
            target,
            json!({
                "type":"rpc", "authorization_body":access.body,
                "invocation":access.authorization.signed_invocation(),
            }),
        )
        .await
    }

    pub(super) async fn manager_access(&self, session: &AgentSession) -> Result<ManagerAccess> {
        self.manager_controls
            .lock()
            .await
            .values()
            .find(|active| {
                active.manager.session.sdk_session == session.sdk_session
                    && active.manager.session.service == session.service
                    && !active.fenced.load(Ordering::Acquire)
            })
            .map(|active| active.manager.clone())
            .context("management invocation is no longer active")
    }

    pub(super) async fn cancel_manager(&self, access: &Access, input: &Value) -> Result<Value> {
        access.require_body(input)?;
        ensure!(
            access.account_actor(),
            "only the user stops a management reply"
        );
        let scope = super::field(input, "conversation_id")?;
        let trigger = super::field(input, "trigger")?;
        self.require_scope(access, scope).await?;
        let network = &access.network_id;
        let target = self.manager_target(network, scope, trigger).await?;
        if target != self.identity.node_id()? {
            self.require_role(network, NodeRole::Gateway).await?;
            return self.forward_manager_control(access, &target).await;
        }
        let job = {
            let mut store = self.store.lock().await;
            let job = store.admit_manager_control(
                &access.authorization,
                &access.freshness,
                &access.transport,
                input,
                super::now_ms() as u64,
            )?;
            if store.scheduler_publication_pending(network, scope, &target, trigger)? {
                return Ok(json!({"job":job,"cancel_requested":false,"publication_pending":true}));
            }
            job
        };
        if let JobState::Cancelling { attempt } = &job.state {
            self.fence_manager_attempt(&attempt.id).await?;
        }
        let cancel_requested = matches!(
            job.state,
            JobState::Cancelling { .. } | JobState::Cancelled { .. }
        );
        self.publish_changed(network, scope, trigger);
        Ok(json!({"job":job,"cancel_requested":cancel_requested}))
    }

    pub(super) async fn resolve_manager_invocation(
        &self,
        access: &Access,
        input: &Value,
    ) -> Result<Value> {
        access.require_body(input)?;
        ensure!(
            access.account_actor(),
            "only the user resolves an unknown management reply"
        );
        let network = &access.network_id;
        let scope = super::field(input, "conversation_id")?;
        let trigger = super::field(input, "trigger")?;
        super::field(input, "evidence")?;
        self.require_scope(access, scope).await?;
        let target = self.manager_target(network, scope, trigger).await?;
        if target != self.identity.node_id()? {
            self.require_role(network, NodeRole::Gateway).await?;
            return self.forward_manager_control(access, &target).await;
        }
        let job = self
            .store
            .lock()
            .await
            .scheduler_job(network, scope, trigger, &target)?
            .context("management job missing")?;
        ensure!(
            matches!(job.state, JobState::Unknown { .. }),
            "management job is not unknown"
        );
        let attempt = &job
            .state
            .attempt()
            .context("management attempt missing")?
            .id;
        self.fence_manager_attempt(attempt).await?;
        let refs = self
            .store
            .lock()
            .await
            .local_value(network, &format!("manager-tools:v1:{attempt}"))?
            .unwrap_or(json!({}));
        for reference in refs
            .as_object()
            .context("invalid management tool references")?
            .values()
        {
            let operation = super::field(reference, "operation_id")?;
            let executor = super::field(reference, "target_node")?;
            let original = self
                .store
                .lock()
                .await
                .context(network, scope, &[])?
                .events
                .into_iter()
                .find_map(|event| match event.body {
                    EventBody::DispatchRecorded {
                        operation_id,
                        action,
                        ..
                    } if event.scope_id == scope && operation_id == operation => {
                        Some(serde_json::from_value::<OperationRequest>(action))
                    }
                    _ => None,
                })
                .transpose()?;
            let record: Option<OperationRecord> = if executor == self.identity.node_id()? {
                self.store.lock().await.operation(network, operation)?
            } else {
                let original = original
                    .as_ref()
                    .context("original management dispatch is missing")?;
                let status = self
                    .verified_execution_status(network, scope, original)
                    .await?;
                serde_json::from_value(status["operation"].clone())?
            };
            if let Some(record) = record {
                Self::verify_manager_operation(
                    network,
                    scope,
                    reference,
                    original
                        .as_ref()
                        .context("original management dispatch is missing")?,
                    &record,
                )?;
                ensure!(
                    record.state.terminal(),
                    "reconcile the original execution operation before resolving this management reply"
                );
            } else {
                ensure!(
                    original.is_none(),
                    "original dispatch has no confirmed execution receipt; reconcile it first"
                );
            }
        }
        let mut store = self.store.lock().await;
        store.admit_manager_control(
            &access.authorization,
            &access.freshness,
            &access.transport,
            input,
            super::now_ms() as u64,
        )?;
        drop(store);
        self.publish_changed(network, scope, trigger);
        Ok(json!({"resolved":true,"trigger":trigger}))
    }

    pub(super) fn verify_manager_operation(
        network: &str,
        scope: &str,
        reference: &Value,
        original: &OperationRequest,
        record: &OperationRecord,
    ) -> Result<()> {
        ensure!(
            original.network_id == network
                && original.conversation_id == scope
                && reference["operation_id"] == original.operation_id
                && reference["target_node"] == original.target_node
                && reference["session_id"] == serde_json::to_value(&original.session_id)?
                && reference["task_id"] == serde_json::to_value(&original.task_id)?,
            "management operation reference does not match its original dispatch"
        );
        let mut admitted = original.clone();
        admitted.resources = record.request.resources.clone();
        ensure!(
            record.request == admitted,
            "execution receipt does not match the original management operation"
        );
        Ok(())
    }

    pub(super) fn record_manager_operation(
        store: &mut crate::store::Store,
        access: &Access,
        request: &OperationRequest,
    ) -> Result<()> {
        Self::check_manager_permission(store, access, true, request.task_id.as_deref())?;
        if let Some(manager) = &access.manager {
            let key = format!("manager-tools:v1:{}", manager.attempt_id);
            let mut refs = store
                .local_value(&access.network_id, &key)?
                .unwrap_or(json!({}));
            let reference = json!({"operation_id":request.operation_id,"target_node":request.target_node,"session_id":request.session_id,"task_id":request.task_id});
            if let Some(previous) = refs.get(&request.operation_id) {
                ensure!(
                    previous == &reference,
                    "management operation identity cannot change"
                );
            } else {
                refs[&request.operation_id] = reference;
            }
            store.local_set(&access.network_id, &key, &refs)?;
        }
        Ok(())
    }
}
