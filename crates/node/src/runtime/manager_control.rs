use super::{Access, Runtime, now_ms};
use crate::model::WorkOutcome;
use crate::scheduling::{JobState, PendingCheck};
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
    pub read_only: bool,
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
        let scope = access
            .conversation_id
            .as_deref()
            .context("missing manager scope")?;
        let job = self
            .store
            .lock()
            .await
            .scheduler_job(&access.network_id, scope, trigger)?
            .context("manager job missing")?;
        let attempt = job.state.attempt().context("manager attempt missing")?;
        ensure!(
            attempt.id == attempt_id && attempt.read_only == read_only,
            "manager attempt mismatch"
        );
        access.manager = Some(ManagerAccess {
            attempt_id: attempt_id.into(),
            trigger: trigger.into(),
            read_only,
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
                &manager.trigger,
                &manager.attempt_id,
                write
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
        self.publish_changed(network, scope, trigger);
        Ok(json!({"job":job,"cancel_requested":cancel_requested}))
    }

    /// Pending checks belong to this Node: an unconfirmed coordination process (`manager:`)
    /// or a work whose real result cannot be proven (`work:`).
    pub(super) async fn pending_check_control(
        self: &Arc<Self>,
        access: &Access,
        input: &Value,
        kind: &str,
    ) -> Result<Value> {
        ensure!(access.human, "only the user reconciles pending checks");
        let scope = super::field(input, "conversation_id")?;
        let check_id = super::field(input, "check_id")?;
        self.require_scope(access, scope).await?;
        let network = &access.network_id;
        if let Some(work_id) = check_id.strip_prefix("work:") {
            return self
                .work_check_control(network, scope, work_id, input, kind)
                .await;
        }
        let check = self.local_pending_check(network, scope, check_id).await?;
        let processes = process_groups(&check.process_groups).await;
        if kind == "reconcile" {
            return Ok(
                json!({"check_id":check_id,"pending":true,"check":check,"process_groups":processes,
                "note":"Exited process groups do not prove that detached tools or external side effects stopped."}),
            );
        }
        let evidence = evidence(input)?;
        let running = processes
            .into_iter()
            .filter(|group| group["exited"] != true)
            .filter_map(|group| group["process_group"].as_u64())
            .collect::<Vec<_>>();
        ensure!(
            running.is_empty(),
            "process groups {running:?} are still running or cannot be checked; stop them before confirming"
        );
        self.fence_manager_attempt(&check.attempt_id).await?;
        self.store
            .lock()
            .await
            .resolve_pending_check(network, check_id, evidence)?;
        self.publish_changed(network, scope, &check.trigger);
        Ok(json!({"check_id":check_id,"resolved":true}))
    }

    /// An unverified work result is checked again on the device that ran it; the user's choice
    /// is signed and written by whichever entry they use.
    async fn work_check_control(
        self: &Arc<Self>,
        network: &str,
        scope: &str,
        work_id: &str,
        input: &Value,
        kind: &str,
    ) -> Result<Value> {
        let check_id = format!("work:{work_id}");
        let work = self
            .store
            .lock()
            .await
            .works(network, scope)?
            .into_iter()
            .find(|work| work.work_id == work_id)
            .context("work not found in this conversation")?;
        if kind == "reconcile" {
            if work.target_node != self.identity.device_id()? {
                return self
                    .peer_request(
                        network,
                        &work.target_node,
                        json!({"type":"reconcile","conversation_id":scope,"check_id":check_id,"human":true}),
                    )
                    .await;
            }
            self.probe_work(network, work_id).await?;
            let record = self.store.lock().await.work_record(network, work_id)?;
            return Ok(
                json!({"check_id":check_id,"pending":record.as_ref().is_none_or(|r| !r.state.finished()),"work":record}),
            );
        }
        let outcome = match super::field(input, "outcome")? {
            "completed" => WorkOutcome::Completed,
            "interrupted" => WorkOutcome::Interrupted,
            _ => bail!("choose completed or interrupted"),
        };
        let note = input["note"].as_str().unwrap_or("").trim();
        ensure!(note.len() <= 4096, "note must be at most 4096 bytes");
        // An agent task's coordination process has its own check here; the work card answers both.
        let manager = self
            .store
            .lock()
            .await
            .pending_checks(network)?
            .into_iter()
            .find(|check| check.trigger == crate::scheduling::task_trigger(work_id));
        if let Some(check) = manager {
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
            self.fence_manager_attempt(&check.attempt_id).await?;
            let evidence = format!("{outcome:?}: {note}");
            self.store.lock().await.resolve_pending_check(
                network,
                &check.check_id,
                if note.is_empty() {
                    "settled with the work's own check"
                } else {
                    &evidence
                },
            )?;
        }
        let receipt = self.store.lock().await.settle_work(
            &self.identity,
            network,
            scope,
            work_id,
            outcome,
            note,
            super::attestation(input)?,
            now_ms(),
        )?;
        self.publish_changed(network, scope, work_id);
        if receipt.is_some() {
            self.spawn_scheduler(network, scope)?;
        }
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
            check.conversation_id == scope,
            "pending check belongs to another conversation"
        );
        Ok(check)
    }
}

fn evidence(input: &Value) -> Result<&str> {
    let evidence = super::field(input, "evidence")?;
    ensure!(
        (12..=4096).contains(&evidence.trim().len()),
        "evidence must be 12 to 4096 bytes"
    );
    Ok(evidence)
}

/// Signal 0 only checks the recorded group; it never stops a group whose ID may have been reused.
async fn process_groups(groups: &[u32]) -> Vec<Value> {
    let mut result = Vec::new();
    for group in groups {
        let probe = tokio::process::Command::new("/bin/kill")
            .args(["-0", "--", &format!("-{group}")])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .await;
        result.push(json!({"process_group":group,"exited":matches!(probe, Ok(status) if !status.success())}));
    }
    result
}
