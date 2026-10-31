use super::{Access, Changed, Runtime, now_ms};
use crate::model::{WorkKind, WorkOutcome, WorkProjection};
use crate::scheduling::{JobOutcome, JobState, PendingCheck, SchedulerJob, TriggerKind};
use crate::work::{Probe, WorkState};
use anyhow::{Context, Result, bail, ensure};
use serde_json::{Value, json};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use tokio::sync::{OwnedRwLockReadGuard, RwLock, watch};
use vibeface_bridge::{
    failure::{ExecFailure, FailureKind},
    turn::TurnStatus,
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ManagerAccess {
    pub attempt_id: String,
    pub trigger: String,
    pub permission_version: u64,
}

/// How long a stop waits for the conversation's owner, which first drains the reply's tools.
const OWNER_STOP_WAIT: std::time::Duration = std::time::Duration::from_secs(10);

pub(crate) struct ActiveManager {
    access: Access,
    /// The provider the reply runs on, whatever the device switched to since.
    provider: crate::providers::Provider,
    token: String,
    cancel: watch::Sender<bool>,
    fenced: AtomicBool,
    admission: Arc<RwLock<()>>,
}

impl Runtime {
    pub(crate) async fn finish_incompatible_tasks(
        &self,
        network: &str,
        conversation: Option<&str>,
    ) -> Result<Vec<SchedulerJob>> {
        let _controls = self.manager_controls.lock().await;
        let current = self.manager().map(|manager| manager.provider);
        let providers = self.providers();
        let ended = self.store.lock().await.refuse_queued_tasks(
            &self.identity,
            network,
            conversation,
            crate::scheduler::boot_id(),
            now_ms(),
            |accepted| match crate::scheduler::task_start_failure(current, accepted.provider) {
                Some(failure) => Ok(Some((
                    self.provider_generation(accepted.provider, &providers)?,
                    failure,
                ))),
                None => Ok(None),
            },
        )?;
        for job in &ended {
            self.publish_changed(network, &job.conversation_id, Changed::Job(&job.trigger));
            self.publish_changed(network, &job.conversation_id, Changed::Work(&job.reply_to));
        }
        if !ended.is_empty() {
            self.wake_scheduler()?;
        }
        Ok(ended)
    }

    pub async fn register_manager_attempt(
        &self,
        token: String,
        mut access: Access,
        attempt_id: &str,
        trigger: &str,
        provider: crate::providers::Provider,
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
            permission_version: attempt.permission_version,
        });
        Self::check_manager_permission(
            &*self.store.lock().await,
            &self.identity.device_id()?,
            &access,
        )?;
        if job.kind == TriggerKind::AgentTask {
            let runtime = job.runtime.as_ref().context("task runtime missing")?;
            ensure!(
                runtime.provider == provider,
                "task runtime provider mismatch"
            );
            if let Some(failure) = crate::scheduler::task_start_failure(
                self.manager().map(|manager| manager.provider),
                runtime.provider,
            ) {
                return Err(crate::manager::ManagerError {
                    stage: vibeface_bridge::failure::ProviderStage::Validate,
                    outcome: crate::manager::ManagerFailure::Failed,
                    start: crate::manager::StartEvidence::NotStarted,
                    session_id: None,
                    failure,
                }
                .into());
            }
            self.store
                .lock()
                .await
                .mark_started(scope, trigger, attempt_id)?;
        }
        let (cancel, receiver) = watch::channel(false);
        let active = Arc::new(ActiveManager {
            access: access.clone(),
            provider,
            token: token.clone(),
            cancel,
            fenced: AtomicBool::new(false),
            admission: Arc::new(RwLock::new(())),
        });
        self.agent_tokens.lock().await.insert(token, access);
        controls.insert(attempt_id.into(), active);
        Ok(receiver)
    }

    /// The provider a reply of this process runs on.
    pub(super) async fn attempt_provider(
        &self,
        attempt_id: &str,
    ) -> Option<crate::providers::Provider> {
        let controls = self.manager_controls.lock().await;
        controls.get(attempt_id).map(|active| active.provider)
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
            self.store
                .lock()
                .await
                .fence_manager_permission_with_version(
                    active
                        .access
                        .conversation_id
                        .as_deref()
                        .context("manager scope missing")?,
                    &manager.trigger,
                    &manager.attempt_id,
                    manager.permission_version,
                )?;
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
                && active.access.conversation_id == access.conversation_id
                && active.access.manager.as_ref() == Some(manager),
            "management invocation scope mismatch"
        );
        let mut store = self.store.lock().await;
        Self::check_manager_permission(&store, &self.identity.device_id()?, access)?;
        store.record_manager_tool_call(
            access
                .conversation_id
                .as_deref()
                .context("missing manager scope")?,
            &manager.trigger,
            &manager.attempt_id,
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
            store.manager_attempt_allowed_with_version(
                &access.network_id,
                local,
                scope,
                &manager.trigger,
                &manager.attempt_id,
                manager.permission_version,
            )?,
            "management invocation permission is no longer valid"
        );
        Ok(())
    }

    /// The user's stop. A reply this Node does not hold is stopped by the conversation's owner;
    /// an unanswered request there is reported unconfirmed and never sent again.
    pub(super) async fn cancel_manager(&self, access: &Access, input: &Value) -> Result<Value> {
        ensure!(access.human, "only the user stops a management reply");
        let scope = super::field(input, "conversation_id")?;
        let trigger = super::field(input, "trigger")?;
        self.require_scope(access, scope).await?;
        let network = &access.network_id;
        let local = self.identity.device_id()?;
        let remote = {
            let store = self.store.lock().await;
            let held = store
                .scheduler_job(scope, trigger)?
                .is_some_and(|job| !job.state.terminal());
            let owner = store
                .conversation(network, scope)?
                .and_then(|conversation| conversation.owner);
            owner.filter(|owner| !held && owner != &local)
        };
        let Some(owner) = remote else {
            return self
                .cancel_local_manager(network, scope, trigger)
                .await?
                .context("this reply is no longer queued here; refresh the conversation");
        };
        let request =
            json!({"type":"cancel_manager","human":true,"conversation_id":scope,"trigger":trigger});
        match tokio::time::timeout(OWNER_STOP_WAIT, self.peer_request(network, &owner, request))
            .await
        {
            Ok(Ok(mut receipt)) => {
                receipt["owner"] = json!(owner);
                Ok(receipt)
            }
            outcome => {
                if let Ok(Err(error)) = outcome {
                    tracing::debug!(%error, "stop sent to the conversation's owner is unconfirmed");
                }
                Ok(json!({"cancel_requested":false,"unconfirmed":true,"owner":owner}))
            }
        }
    }

    /// Stops a reply this Node schedules; `None` when it holds no such job.
    pub(super) async fn cancel_local_manager(
        &self,
        network: &str,
        scope: &str,
        trigger: &str,
    ) -> Result<Option<Value>> {
        let Some(job) = self.store.lock().await.request_manager_cancel(
            &self.identity,
            network,
            scope,
            trigger,
        )?
        else {
            return Ok(None);
        };
        if let JobState::Cancelling { attempt } = &job.state {
            self.fence_manager_attempt(&attempt.id).await?;
        }
        let cancel_requested = matches!(
            job.state,
            JobState::Cancelling { .. } | JobState::Cancelled { .. }
        );
        self.publish_changed(network, scope, Changed::Job(trigger));
        Ok(Some(json!({"job":job,"cancel_requested":cancel_requested})))
    }

    pub(super) async fn reconcile_checks(self: &Arc<Self>, network: &str) -> Result<()> {
        let checks = self.store.lock().await.pending_checks()?;
        for check in checks {
            let Some((outcome, text)) = self.read_back_reply(&check).await else {
                continue;
            };
            self.fence_manager_attempt(&check.attempt_id).await?;
            if self
                .store
                .lock()
                .await
                .scheduler_job(&check.conversation_id, &check.trigger)?
                .is_some_and(|job| job.runtime.is_some())
            {
                self.store.lock().await.update_turn_runtime(
                    &check.conversation_id,
                    &check.trigger,
                    &check.attempt_id,
                    vibeface_bridge::failure::ProviderStage::ReadBack,
                    crate::scheduling::TurnExecution::Started,
                )?;
            }
            self.store.lock().await.finish_job(
                &self.identity,
                network,
                &check.conversation_id,
                &check.trigger,
                &check.attempt_id,
                outcome,
                &text,
                now_ms(),
            )?;
            self.store
                .lock()
                .await
                .resolve_pending_check(&check.attempt_id)?;
            self.codex.release_manager(&check.attempt_id);
            if let Some(turn) = &check.codex_turn
                && turn.server == super::opencode::SERVER
            {
                self.opencode.release_manager(&turn.thread);
            }
            self.publish_changed(
                network,
                &check.conversation_id,
                Changed::Job(&check.trigger),
            );
            self.wake_scheduler()?;
        }
        Ok(())
    }

    async fn read_back_reply(&self, check: &PendingCheck) -> Option<(JobOutcome, String)> {
        let turn = check.codex_turn.as_ref()?;
        let job = self
            .store
            .lock()
            .await
            .scheduler_job(&check.conversation_id, &check.trigger)
            .ok()??;
        let runtime = job.runtime.as_ref()?;
        let probe = match runtime.provider {
            crate::providers::Provider::Codex if turn.server == self.codex.server() => self
                .codex
                .probe(&turn.thread, turn.turn.as_deref(), &check.attempt_id)
                .await
                .ok()?,
            crate::providers::Provider::Opencode if turn.server == super::opencode::SERVER => self
                .opencode
                .probe_reply(
                    &turn.thread,
                    &turn
                        .turn
                        .clone()
                        .unwrap_or_else(|| super::opencode::reply_message(&check.attempt_id)),
                )
                .await
                .ok()?,
            crate::providers::Provider::Claude => self
                .claude
                .probe_reply(&turn.thread, turn.turn.as_deref(), &check.attempt_id)
                .await
                .ok()?,
            crate::providers::Provider::Pi => self
                .pi
                .probe_reply(&turn.thread, turn.turn.as_deref(), &check.attempt_id)
                .await
                .ok()?,
            crate::providers::Provider::Cursor => super::cursor::probe_recorded_reply(
                self,
                &check.attempt_id,
                &turn.thread,
                turn.turn.as_deref()?,
            )
            .await
            .ok()?,
            _ => return None,
        };
        let Probe::Terminal {
            status,
            text,
            error,
            ..
        } = probe
        else {
            return None;
        };
        let outcome = match status {
            TurnStatus::Completed => JobOutcome::Completed,
            TurnStatus::Failed => JobOutcome::Failed(ExecFailure::new(
                error.map_or(FailureKind::Unknown, |failure| failure.kind),
                "the provider recorded a failed turn",
            )),
            TurnStatus::Interrupted => JobOutcome::Interrupted {
                failure: ExecFailure::new(
                    error.map_or(FailureKind::Lost, |failure| failure.kind),
                    "the provider confirmed this turn was interrupted",
                ),
                process_unconfirmed: false,
            },
            TurnStatus::InProgress | TurnStatus::Other(_) => return None,
        };
        let secrets: Vec<_> = crate::providers::Provider::ALL
            .into_iter()
            .flat_map(|provider| self.provider_env(provider).into_values())
            .chain(self.agent_env().into_values())
            .chain(
                std::env::vars()
                    .filter(|(name, _)| crate::logging::credential_name(name))
                    .map(|(_, value)| value),
            )
            .filter(|value| value.len() >= 8)
            .collect();
        Some((outcome, crate::manager::redact_string(text, &secrets)))
    }

    async fn close_check(&self, network: &str, check: &PendingCheck) -> Result<()> {
        if check
            .codex_turn
            .as_ref()
            .is_some_and(|turn| !turn.server.is_empty())
        {
            ensure!(
                !check.process_groups.iter().any(|group| group_alive(*group)),
                "仍有协调进程在运行，请先停止它再确认。"
            );
        }
        // The owner's stopped confirmation remains authoritative when the provider is unreachable.
        let observed_running = tokio::time::timeout(std::time::Duration::from_secs(3), async {
            let Some(turn) = &check.codex_turn else {
                return false;
            };
            if turn.server == super::opencode::SERVER {
                return self.opencode.manager_running(&turn.thread).await == Some(true);
            }
            !turn.server.is_empty()
                && turn.server == self.codex.server()
                && matches!(
                    self.codex
                        .probe(&turn.thread, turn.turn.as_deref(), &check.attempt_id)
                        .await,
                    Ok(Probe::Running { .. })
                )
        })
        .await
        .unwrap_or(false);
        ensure!(
            !observed_running,
            "Provider 仍在运行或有排队输入，请先停止后再确认。"
        );
        self.fence_manager_attempt(&check.attempt_id).await?;
        self.store.lock().await.confirm_manager_stopped(
            &self.identity,
            network,
            check,
            now_ms(),
        )?;
        self.codex.release_manager(&check.attempt_id);
        if let Some(turn) = &check.codex_turn
            && turn.server == super::opencode::SERVER
        {
            self.opencode.release_manager(&turn.thread);
        }
        self.wake_scheduler()?;
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
    /// reachable one; a work on the App Server also needs that device's App Server connected.
    async fn target_vouches(&self, network: &str, work: &WorkProjection) -> Result<bool> {
        let codex = if work.target_node == self.identity.device_id()? {
            let store = self.store.lock().await;
            if store
                .pending_checks()?
                .iter()
                .any(|check| check.trigger == crate::scheduling::task_trigger(&work.work_id))
            {
                return Ok(false);
            }
            let run = store.work_record(&work.work_id)?;
            drop(store);
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
        Ok(match work.kind {
            WorkKind::Agent(provider) if provider.on_app_server() => codex["connected"] == true,
            _ => true,
        })
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
                "它的设备还在线。等它报回结果就行。"
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
        if let Some(check) = manager {
            self.close_check(network, &check).await?;
        }
        self.publish_changed(network, scope, Changed::Work(work_id));
        if receipt.is_some() {
            self.wake_scheduler()?;
        }
        Ok(json!({"check_id":format!("work:{work_id}"),"resolved":true}))
    }
}

/// Signal 0 only asks whether the recorded group still exists; it never signals the processes.
pub(super) fn group_alive(group: u32) -> bool {
    let Ok(group) = libc::pid_t::try_from(group) else {
        return false;
    };
    // SAFETY: kill with signal 0 performs only the existence and permission check.
    let result = unsafe { libc::kill(-group, 0) };
    result == 0 || std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;

    pub(in crate::runtime) async fn pending_manager(
        node: &Runtime,
        admin: &crate::crypto::LocalIdentity,
        server: &str,
        thread: &str,
    ) -> Result<(Access, PendingCheck)> {
        super::super::triage::tests::bot(node, admin, "c", "fixture", &[]).await?;
        let (trigger, attempt) =
            super::super::triage::tests::start_reply(node, admin, "confirm-stop").await?;
        let network = node.store.lock().await.network()?.context("network")?;
        let mut store = node.store.lock().await;
        store.record_manager_turn(&attempt, server, thread, Some("turn"), &Default::default())?;
        store.finish_job(
            &node.identity,
            &network,
            "c",
            &trigger,
            &attempt,
            crate::scheduling::JobOutcome::Interrupted {
                failure: vibeface_bridge::failure::ExecFailure::new(
                    vibeface_bridge::failure::FailureKind::Lost,
                    "fixture lost its turn",
                ),
                process_unconfirmed: true,
            },
            "",
            now_ms(),
        )?;
        let check = store
            .pending_checks()?
            .into_iter()
            .find(|check| check.attempt_id == attempt)
            .context("fixture pending check")?;
        Ok((
            Access {
                network_id: network,
                human: true,
                conversation_id: Some("c".into()),
                manager: None,
            },
            check,
        ))
    }
}
