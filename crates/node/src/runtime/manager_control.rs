use super::native_observation::NativeState;
use super::{Access, Runtime};
use crate::{
    model::*,
    scheduling::{JobState, PendingCheck},
};
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
}

pub(super) struct ActiveManager {
    access: Access,
    token: String,
    cancel: watch::Sender<bool>,
    fenced: AtomicBool,
    admission: Arc<RwLock<()>>,
}

impl Runtime {
    pub async fn register_manager_attempt(
        &self,
        token: String,
        mut access: Access,
        attempt_id: &str,
        trigger: &str,
        read_only: bool,
    ) -> Result<watch::Receiver<bool>> {
        let mut controls = self.manager_controls.lock().await;
        if let Some(active) = controls.get(attempt_id) {
            return Ok(active.cancel.subscribe());
        }
        let target = self.identity.peer_id()?;
        let scope = access
            .conversation_id
            .as_deref()
            .context("missing manager scope")?;
        let job = self
            .store
            .lock()
            .await
            .scheduler_job(&access.network_id, scope, trigger, &target)?
            .context("manager job missing")?;
        let attempt = job.state.attempt().context("manager attempt missing")?;
        ensure!(
            attempt.id == attempt_id && attempt.read_only == read_only,
            "manager attempt mismatch"
        );
        access.manager = Some(ManagerAccess {
            attempt_id: attempt_id.into(),
            trigger: trigger.into(),
            target_node: target,
            read_only,
            followup_version: attempt.followup_version,
        });
        let (cancel, receiver) = watch::channel(false);
        let active = Arc::new(ActiveManager {
            access: access.clone(),
            token: token.clone(),
            cancel,
            fenced: AtomicBool::new(false),
            admission: Arc::new(RwLock::new(())),
        });
        self.agent_tokens.lock().await.insert(token, access);
        controls.insert(attempt_id.into(), active);
        Ok(receiver)
    }

    pub async fn fence_manager_attempt(&self, attempt_id: &str) -> Result<()> {
        if let Some(active) = self.manager_controls.lock().await.get(attempt_id).cloned() {
            let _ = active.cancel.send(true);
        }
        self.seal_manager_tools(attempt_id).await
    }

    pub(crate) async fn seal_manager_tools(&self, attempt_id: &str) -> Result<()> {
        let active = self.manager_controls.lock().await.get(attempt_id).cloned();
        if let Some(active) = active {
            active.fenced.store(true, Ordering::Release);
            self.agent_tokens.lock().await.remove(&active.token);
            let manager = active
                .access
                .manager
                .as_ref()
                .context("manager metadata missing")?;
            self.store.lock().await.fence_manager_permission(
                &active.access.network_id,
                active
                    .access
                    .conversation_id
                    .as_deref()
                    .context("manager scope missing")?,
                &manager.target_node,
                &manager.trigger,
                &manager.attempt_id,
            )?;
            // Wait for admitted tools before declaring this attempt unable to write.
            let _drained = active.admission.clone().write_owned().await;
            self.clear_agent_blob_attempt(attempt_id).await;
        }
        Ok(())
    }

    pub async fn finish_manager_attempt(&self, attempt_id: &str) {
        let active = self.manager_controls.lock().await.get(attempt_id).cloned();
        if let Some(active) = active {
            active.fenced.store(true, Ordering::Release);
            self.agent_tokens.lock().await.remove(&active.token);
            let _drained = active.admission.clone().write_owned().await;
            self.clear_agent_blob_attempt(attempt_id).await;
            self.manager_controls.lock().await.remove(attempt_id);
        }
    }

    pub(super) async fn manager_tool_lease(
        &self,
        access: &Access,
        write: bool,
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
            active.access.network_id == access.network_id
                && active.access.conversation_id == access.conversation_id,
            "management invocation scope mismatch"
        );
        ensure!(
            !write || !manager.read_only,
            "this management invocation is read-only"
        );
        Self::check_manager_permission(&*self.store.lock().await, access, write)?;
        Ok(Some(guard))
    }

    pub(super) fn check_manager_permission(
        store: &crate::store::Store,
        access: &Access,
        write: bool,
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
            store.validate_latest_manager_policy(
                &access.network_id,
                scope,
                &manager.trigger,
                &manager.target_node,
                version,
                write,
            )?;
        }
        Ok(())
    }

    async fn manager_target(&self, network: &str, scope: &str, trigger: &str) -> Result<String> {
        let local = self.identity.peer_id()?;
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
            EventBody::AgentMessage { message } => Ok(message.target_node.clone()),
            _ => anyhow::bail!("event is not a management trigger"),
        }
    }

    pub(super) async fn cancel_manager(&self, access: &Access, input: &Value) -> Result<Value> {
        ensure!(access.human, "only the user stops a management reply");
        let scope = super::field(input, "conversation_id")?;
        let trigger = super::field(input, "trigger")?;
        self.require_scope(access, scope).await?;
        let network = &access.network_id;
        let target = self.manager_target(network, scope, trigger).await?;
        if target != self.identity.peer_id()? {
            self.require_role(network, NodeRole::Gateway).await?;
            return self.peer_request(network, &target, json!({"type":"cancel_manager","conversation_id":scope,"trigger":trigger,"human":true})).await;
        }
        let job = {
            let mut store = self.store.lock().await;
            let job = store
                .request_manager_cancel(network, scope, &target, trigger)?
                .context("management job missing")?;
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

    /// Routes a pending check to the Node that owns it; only that Node can observe the original work.
    pub(super) async fn pending_check_control(
        self: &Arc<Self>,
        access: &Access,
        input: &Value,
        kind: &str,
    ) -> Result<Value> {
        ensure!(access.human, "only the user reconciles pending checks");
        let scope = super::field(input, "conversation_id")?;
        let target = super::field(input, "target_node")?;
        super::field(input, "check_id")?;
        self.require_scope(access, scope).await?;
        if target != self.identity.peer_id()? {
            self.require_role(&access.network_id, NodeRole::Gateway)
                .await?;
            let mut request = input.clone();
            request["type"] = json!(kind);
            request["human"] = json!(true);
            return self.peer_request(&access.network_id, target, request).await;
        }
        if kind == "reconcile" {
            self.reconcile_local(&access.network_id, input).await
        } else {
            self.resolve_local(&access.network_id, input).await
        }
    }

    pub(super) async fn reconcile_local(&self, network: &str, input: &Value) -> Result<Value> {
        let scope = super::field(input, "conversation_id")?;
        let check_id = super::field(input, "check_id")?;
        if let Some(work_id) = check_id.strip_prefix("native:") {
            let work = self.reconcile_native_work(network, scope, work_id).await?;
            self.publish_changed(network, scope, &work.origin_trigger);
            return Ok(
                json!({"check_id":check_id,"pending":work.state == NativeState::Unknown,"native_session":work}),
            );
        }
        let check = self.local_pending_check(network, scope, check_id).await?;
        let processes = process_groups(&check.process_groups).await;
        let native = self
            .native_work_for_attempt(network, scope, &check.trigger, &check.attempt_id)
            .await?;
        let tasks = self
            .child_agent_tasks(network, scope, &check.trigger)
            .await?;
        Ok(
            json!({"check_id":check_id,"pending":true,"check":check,"process_groups":processes,
            "native_sessions":native,"agent_tasks":tasks,
            "note":"Exited process groups do not prove that detached tools or external side effects stopped."}),
        )
    }

    pub(super) async fn resolve_local(&self, network: &str, input: &Value) -> Result<Value> {
        let scope = super::field(input, "conversation_id")?;
        let check_id = super::field(input, "check_id")?;
        let evidence = super::field(input, "evidence")?;
        ensure!(
            (12..=4096).contains(&evidence.trim().len()),
            "evidence must be 12 to 4096 bytes"
        );
        if let Some(work_id) = check_id.strip_prefix("native:") {
            let state = match super::field(input, "outcome")? {
                "completed" => NativeState::Completed,
                "failed" => NativeState::Failed,
                "interrupted" => NativeState::Interrupted,
                _ => anyhow::bail!("outcome must be completed, failed or interrupted"),
            };
            let work = self
                .resolve_native_work(network, scope, work_id, state, evidence)
                .await?;
            self.publish_changed(network, scope, &work.origin_trigger);
            return Ok(json!({"check_id":check_id,"resolved":true}));
        }
        let check = self.local_pending_check(network, scope, check_id).await?;
        let running = process_groups(&check.process_groups)
            .await
            .into_iter()
            .filter(|group| group["exited"] != true)
            .filter_map(|group| group["process_group"].as_u64())
            .collect::<Vec<_>>();
        ensure!(
            running.is_empty(),
            "process groups {running:?} are still running or cannot be checked; stop them before confirming"
        );
        let native = self
            .native_work_for_attempt(network, scope, &check.trigger, &check.attempt_id)
            .await?;
        ensure!(
            native.iter().all(|work| work.state.terminal()),
            "confirm the native turns this invocation started before confirming the invocation"
        );
        self.fence_manager_attempt(&check.attempt_id).await?;
        self.store.lock().await.resolve_pending_check(
            network,
            check_id,
            evidence,
            super::now_ms(),
        )?;
        self.publish_changed(network, scope, &check.trigger);
        Ok(json!({"check_id":check_id,"resolved":true}))
    }

    async fn local_pending_check(
        &self,
        network: &str,
        scope: &str,
        check_id: &str,
    ) -> Result<PendingCheck> {
        let check = self
            .store
            .lock()
            .await
            .pending_check(network, check_id)?
            .context("pending check not found")?;
        ensure!(
            check.conversation_id == scope && check.target_node == self.identity.peer_id()?,
            "pending check belongs to another conversation or device"
        );
        Ok(check)
    }
}

/// Signal 0 only checks the recorded group; it never stops a group whose ID may have been reused.
async fn process_groups(groups: &[u32]) -> Vec<Value> {
    let mut result = Vec::new();
    for group in groups {
        let probe = tokio::process::Command::new("/bin/kill")
            .args(["-0", &format!("-{group}")])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .await;
        result.push(json!({"process_group":group,"exited":matches!(probe, Ok(status) if !status.success())}));
    }
    result
}
