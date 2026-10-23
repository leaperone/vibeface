use super::{Access, Changed, Runtime, now_ms};
use crate::model::{WorkKind, WorkOutcome, WorkProjection};
use crate::scheduling::{JobState, PendingCheck, TaskJobChange, task_work_signal};
use crate::work::WorkState;
use anyhow::{Context, Result, bail, ensure};
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
    ) -> Result<watch::Receiver<bool>> {
        let mut controls = self.manager_controls.lock().await;
        if let Some(active) = controls.get(attempt_id) {
            return Ok(active.cancel.subscribe());
        }
        let scope = access
            .conversation_id
            .as_deref()
            .context("missing manager scope")?;
        let job = self
            .store
            .lock()
            .await
            .scheduler_job(scope, trigger)?
            .context("manager job missing")?;
        let attempt = job.state.attempt().context("manager attempt missing")?;
        ensure!(attempt.id == attempt_id, "manager attempt mismatch");
        access.manager = Some(ManagerAccess {
            attempt_id: attempt_id.into(),
            trigger: trigger.into(),
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
            self.revoke_tool_token(&active.token).await;
            let manager = active
                .access
                .manager
                .as_ref()
                .context("manager metadata missing")?;
            self.store.lock().await.fence_manager_permission(
                active
                    .access
                    .conversation_id
                    .as_deref()
                    .context("manager scope missing")?,
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
            self.revoke_tool_token(&active.token).await;
            let _drained = active.admission.clone().write_owned().await;
            self.manager_controls.lock().await.remove(attempt_id);
        }
    }

    pub(super) async fn revoke_tool_token(&self, token: &str) {
        self.agent_tokens.lock().await.remove(token);
        self.cursor_agents
            .lock()
            .await
            .retain(|_, attempt| attempt != token);
    }

    pub(super) async fn manager_tool_lease(
        &self,
        access: &Access,
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
        Self::check_manager_permission(
            &*self.store.lock().await,
            &self.identity.device_id()?,
            access,
        )?;
        Ok(Some(guard))
    }

    pub(super) fn check_manager_permission(
        store: &crate::store::Store,
        local: &str,
        access: &Access,
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
                local,
                scope,
                &manager.trigger,
                &manager.attempt_id,
            )?,
            "management invocation permission is no longer valid"
        );
        Ok(())
    }

    pub(super) async fn cancel_manager(&self, access: &Access, input: &Value) -> Result<Value> {
        ensure!(access.human, "only the user stops a management reply");
        let scope = super::field(input, "conversation_id")?;
        let trigger = super::field(input, "trigger")?;
        self.require_scope(access, scope).await?;
        let network = &access.network_id;
        let job = self
            .store
            .lock()
            .await
            .request_manager_cancel(&self.identity, network, scope, trigger)?
            .context("this reply is scheduled on another device; stop it there")?;
        if let JobState::Cancelling { attempt } = &job.state {
            self.fence_manager_attempt(&attempt.id).await?;
        }
        let cancel_requested = matches!(
            job.state,
            JobState::Cancelling { .. } | JobState::Cancelled { .. }
        );
        self.publish_changed(network, scope, Changed::Job(trigger));
        Ok(json!({"job":job,"cancel_requested":cancel_requested}))
    }

    /// A coordination process left over from a crash is checked by its recorded process groups.
    /// Once every group has exited the check resolves itself; an agent task waiting on it ends
    /// as interrupted. Only a group that is still alive leaves a card for the user.
    pub(super) async fn reconcile_checks(self: &Arc<Self>, network: &str) -> Result<()> {
        let checks = self.store.lock().await.pending_checks()?;
        for check in checks {
            if check.process_groups.iter().any(|group| group_alive(*group)) {
                continue;
            }
            self.close_check(network, &check).await?;
            if let Some((work_id, signal)) =
                task_work_signal(&check.trigger, TaskJobChange::ProcessExited)
                && self
                    .store
                    .lock()
                    .await
                    .work_record(work_id)?
                    .is_some_and(|record| {
                        matches!(record.state, WorkState::Unverified { handle: None, .. })
                    })
            {
                self.signal_work(network, work_id, signal).await?;
            }
        }
        Ok(())
    }

    async fn close_check(&self, network: &str, check: &PendingCheck) -> Result<()> {
        self.fence_manager_attempt(&check.attempt_id).await?;
        self.store
            .lock()
            .await
            .resolve_pending_check(&check.attempt_id)?;
        self.publish_changed(
            network,
            &check.conversation_id,
            Changed::Job(&check.trigger),
        );
        Ok(())
    }

    /// The user's one tap on a card the Node could not settle: a coordination process they
    /// confirm is stopped, or the signed result of an unverified work.
    pub(super) async fn resolve_check(
        self: &Arc<Self>,
        access: &Access,
        input: &Value,
    ) -> Result<Value> {
        ensure!(access.human, "only the user resolves pending checks");
        let scope = super::field(input, "conversation_id")?;
        let check_id = super::field(input, "check_id")?;
        self.require_scope(access, scope).await?;
        let network = &access.network_id;
        if let Some(work_id) = check_id.strip_prefix("work:") {
            return self.settle_work_check(network, scope, work_id, input).await;
        }
        let check = self
            .store
            .lock()
            .await
            .pending_checks()?
            .into_iter()
            .find(|check| check.check_id == check_id && check.conversation_id == scope)
            .context("pending check not found")?;
        self.close_check(network, &check).await?;
        Ok(json!({"check_id":check_id,"resolved":true}))
    }

    /// Whether the work's device can still report it: this Node with the run on record, or a
    /// reachable one; a Codex work also needs that device's App Server connected.
    async fn target_vouches(&self, network: &str, work: &WorkProjection) -> Result<bool> {
        let codex = if work.target_node == self.identity.device_id()? {
            let run = self.store.lock().await.work_record(&work.work_id)?;
            if run.is_none_or(|run| matches!(run.state, WorkState::Unverified { .. })) {
                return Ok(false);
            }
            self.codex.capability().await
        } else {
            let request = json!({"type":"capabilities"});
            match tokio::time::timeout(
                std::time::Duration::from_secs(3),
                self.peer_request(network, &work.target_node, request),
            )
            .await
            {
                Ok(Ok(capabilities)) => capabilities["codex"].clone(),
                _ => return Ok(false),
            }
        };
        Ok(work.kind == WorkKind::Task || codex["connected"] == true)
    }

    async fn settle_work_check(
        self: &Arc<Self>,
        network: &str,
        scope: &str,
        work_id: &str,
        input: &Value,
    ) -> Result<Value> {
        let outcome = match super::field(input, "outcome")? {
            "completed" => WorkOutcome::Completed,
            "interrupted" => WorkOutcome::Interrupted,
            _ => bail!("choose completed or interrupted"),
        };
        let note = input["note"].as_str().unwrap_or("").trim();
        ensure!(note.len() <= 4096, "note must be at most 4096 bytes");
        let work = self
            .store
            .lock()
            .await
            .works(network, scope)?
            .into_iter()
            .find(|work| work.work_id == work_id)
            .context("work not found in this conversation")?;
        match work.outcome {
            Some(WorkOutcome::Unverified) => {}
            Some(_) => bail!("这个结果已经不需要你确认了"),
            None => ensure!(
                !self.target_vouches(network, &work).await?,
                "它的设备还能报回结果，请等它报告"
            ),
        }
        let unvouched = work.outcome.is_none();
        // An agent task's coordination process has its own check here; the work card answers both.
        let manager = self
            .store
            .lock()
            .await
            .pending_checks()?
            .into_iter()
            .find(|check| check.trigger == crate::scheduling::task_trigger(work_id));
        if let Some(check) = manager {
            self.close_check(network, &check).await?;
        }
        let receipt = self.store.lock().await.settle_work(
            &self.identity,
            network,
            scope,
            work_id,
            outcome,
            note,
            super::attestation(input)?,
            unvouched,
            now_ms(),
        )?;
        self.publish_changed(network, scope, Changed::Work(work_id));
        if receipt.is_some() {
            self.wake_scheduler()?;
        }
        Ok(json!({"check_id":format!("work:{work_id}"),"resolved":true}))
    }
}

/// Signal 0 only asks whether the recorded group still exists; it never signals the processes.
fn group_alive(group: u32) -> bool {
    let Ok(group) = libc::pid_t::try_from(group) else {
        return false;
    };
    // SAFETY: kill with signal 0 performs only the existence and permission check.
    let result = unsafe { libc::kill(-group, 0) };
    result == 0 || std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
}
