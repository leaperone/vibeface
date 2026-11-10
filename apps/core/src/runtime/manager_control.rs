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
    pub(super) access: Access,
    /// The provider the reply runs on, whatever the device switched to since.
    pub(super) provider: crate::providers::Provider,
    pub(super) token: String,
    pub(super) cancel: watch::Sender<bool>,
    pub(super) fenced: AtomicBool,
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
        let ended = {
            let identity = self.identity.clone();
            let network = network.to_owned();
            let conversation = conversation.map(str::to_owned);
            self.store
                .call("manager_control.refuse_queued_tasks", move |store| {
                    store.refuse_queued_tasks(
                        &identity,
                        &network,
                        conversation.as_deref(),
                        crate::scheduler::boot_id(),
                        now_ms(),
                        |accepted| match crate::scheduler::task_start_failure(
                            current,
                            accepted.provider,
                        ) {
                            Some(failure) => Ok(Some((
                                super::provider_probe::generation_for(
                                    &identity,
                                    accepted.provider,
                                    &providers,
                                )?,
                                failure,
                            ))),
                            None => Ok(None),
                        },
                    )
                })
                .await
        }?;
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
        captured_permission_epoch: u64,
    ) -> Result<watch::Receiver<bool>> {
        let mut controls = self.manager_controls.lock().await;
        ensure!(self.writable(), "Node execution authority is unavailable");
        ensure!(
            self.local_permission_epoch.load(Ordering::Acquire) == captured_permission_epoch,
            "management permissions changed before this attempt registered"
        );
        if let Some(active) = controls.get(attempt_id) {
            return Ok(active.cancel.subscribe());
        }
        let scope = access
            .conversation_id
            .as_deref()
            .context("missing manager scope")?;
        let job = {
            let arg0 = (scope).to_owned();
            let arg1 = (trigger).to_owned();
            self.store
                .call("manager_control.scheduler_job", move |store| {
                    store.scheduler_job(&arg0, &arg1)
                })
                .await
        }?
        .context("manager job missing")?;
        let attempt = job.state.attempt().context("manager attempt missing")?;
        ensure!(attempt.id == attempt_id, "manager attempt mismatch");
        access.manager = Some(ManagerAccess {
            attempt_id: attempt_id.into(),
            trigger: trigger.into(),
            permission_version: attempt.permission_version,
        });
        {
            let identity = self.identity.clone();
            let access = access.clone();
            self.store
                .call("manager_control.check_permission", move |store| {
                    Self::check_manager_permission(store, &identity.device_id()?, &access)
                })
                .await?;
        }
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
            {
                let arg0 = (scope).to_owned();
                let arg1 = (trigger).to_owned();
                let arg2 = (attempt_id).to_owned();
                self.store
                    .call("manager_control.mark_started", move |store| {
                        store.mark_started(&arg0, &arg1, &arg2)
                    })
                    .await
            }?;
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

    #[cfg(test)]
    pub(crate) async fn register_manager_attempt_fixture(
        &self,
        token: String,
        access: Access,
        attempt_id: &str,
        trigger: &str,
        provider: crate::providers::Provider,
    ) -> Result<watch::Receiver<bool>> {
        self.register_manager_attempt(
            token,
            access,
            attempt_id,
            trigger,
            provider,
            self.local_permission_epoch.load(Ordering::Acquire),
        )
        .await
    }

    #[cfg(test)]
    pub(crate) async fn manager_access_fixture(
        &self,
        token: &str,
        access: Access,
        attempt_id: &str,
        trigger: &str,
        provider: crate::providers::Provider,
    ) -> Result<Access> {
        self.register_manager_attempt_fixture(token.into(), access, attempt_id, trigger, provider)
            .await?;
        self.agent_tokens
            .lock()
            .await
            .get(token)
            .cloned()
            .context("registered tool token")
    }

    /// The provider a reply of this process runs on.
    #[cfg(test)]
    pub(super) async fn attempt_provider(
        &self,
        attempt_id: &str,
    ) -> Option<crate::providers::Provider> {
        let controls = self.manager_controls.lock().await;
        controls.get(attempt_id).map(|active| active.provider)
    }

    pub(crate) async fn manager_input_lease(
        &self,
        attempt_id: &str,
        provider: crate::providers::Provider,
    ) -> Result<OwnedRwLockReadGuard<()>> {
        let active = self
            .manager_controls
            .lock()
            .await
            .get(attempt_id)
            .cloned()
            .context("management invocation is no longer active")?;
        let guard = active.admission.clone().read_owned().await;
        ensure!(
            !active.fenced.load(Ordering::Acquire) && active.provider == provider,
            "management input target is no longer valid"
        );
        let access = active.access.clone();
        let identity = self.identity.clone();
        let write_state = self.write_state.clone();
        self.store
            .call("manager_control.input_lease", move |store| {
                ensure!(
                    *write_state.borrow() == super::WriteState::Writable,
                    "Node execution authority is unavailable"
                );
                let local = identity.device_id()?;
                Self::check_manager_permission(store, &local, &access)?;
                let manager = access
                    .manager
                    .as_ref()
                    .context("manager metadata missing")?;
                let scope = access
                    .conversation_id
                    .as_deref()
                    .context("manager scope missing")?;
                let job = store
                    .scheduler_job(scope, &manager.trigger)?
                    .context("manager job missing")?;
                if let Some(assignment) =
                    crate::store::workspace::workspace_assignment_for_job_from(
                        &store.connection,
                        &access.network_id,
                        &local,
                        &job,
                    )?
                    && let Err(error) = Self::validate_assignment_roots(&assignment.resource_roots)
                {
                    if !job.tool_calls
                        && !job.pending_check
                        && job.runtime.as_ref().is_some_and(|runtime| {
                            runtime.execution == crate::scheduling::TurnExecution::NotStarted
                        })
                    {
                        return Err(crate::manager::ManagerError::not_started(error));
                    }
                    return Err(error);
                }
                Ok(())
            })
            .await?;
        Ok(guard)
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
            {
                let arg0 = (active
                    .access
                    .conversation_id
                    .as_deref()
                    .context("manager scope missing")?)
                .to_owned();
                let arg1 = (manager.trigger).to_owned();
                let arg2 = (manager.attempt_id).to_owned();
                let arg3 = manager.permission_version;
                self.store
                    .call(
                        "manager_control.fence_manager_permission_with_version",
                        move |store| {
                            store.fence_manager_permission_with_version(&arg0, &arg1, &arg2, arg3)
                        },
                    )
                    .await
            }?;
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
        let access = access.clone();
        let manager = manager.clone();
        let identity = self.identity.clone();
        let write_state = self.write_state.clone();
        self.store
            .call("manager_control.tool_lease", move |store| {
                ensure!(
                    *write_state.borrow() == super::WriteState::Writable,
                    "Node execution authority is unavailable"
                );
                Self::check_manager_permission(store, &identity.device_id()?, &access)?;
                store.record_manager_tool_call(
                    access
                        .conversation_id
                        .as_deref()
                        .context("missing manager scope")?,
                    &manager.trigger,
                    &manager.attempt_id,
                )?;
                Ok(())
            })
            .await?;
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
            let scope = scope.to_owned();
            let trigger = trigger.to_owned();
            let network = network.clone();
            let local = local.clone();
            self.store
                .call("manager_control.cancel_route", move |store| {
                    let held = store
                        .scheduler_job(&scope, &trigger)?
                        .is_some_and(|job| !job.state.terminal());
                    let owner = store
                        .conversation(&network, &scope)?
                        .and_then(|conversation| conversation.owner);
                    Ok(owner.filter(|owner| !held && owner != &local))
                })
                .await?
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
        let Some(job) = {
            let arg0 = (self.identity).to_owned();
            let arg1 = (network).to_owned();
            let arg2 = (scope).to_owned();
            let arg3 = (trigger).to_owned();
            self.store
                .call("manager_control.request_manager_cancel", move |store| {
                    store.request_manager_cancel(&arg0, &arg1, &arg2, &arg3)
                })
                .await
        }?
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
        let checks = self
            .store
            .call("manager_control.pending_checks", move |store| {
                store.pending_checks()
            })
            .await?;
        for check in checks {
            let Some((outcome, text)) = self.read_back_reply(&check).await else {
                continue;
            };
            self.fence_manager_attempt(&check.attempt_id).await?;
            if {
                let arg0 = (check.conversation_id).to_owned();
                let arg1 = (check.trigger).to_owned();
                self.store
                    .call("manager_control.scheduler_job", move |store| {
                        store.scheduler_job(&arg0, &arg1)
                    })
                    .await
            }?
            .is_some_and(|job| job.runtime.is_some())
            {
                {
                    let arg0 = (check.conversation_id).to_owned();
                    let arg1 = (check.trigger).to_owned();
                    let arg2 = (check.attempt_id).to_owned();
                    let arg3 = vibeface_bridge::failure::ProviderStage::ReadBack;
                    let arg4 = crate::scheduling::TurnExecution::Started;
                    self.store
                        .call("manager_control.update_turn_runtime", move |store| {
                            store.update_turn_runtime(&arg0, &arg1, &arg2, arg3, arg4)
                        })
                        .await
                }?;
            }
            {
                let arg0 = (self.identity).to_owned();
                let arg1 = (network).to_owned();
                let arg2 = (check.conversation_id).to_owned();
                let arg3 = (check.trigger).to_owned();
                let arg4 = (check.attempt_id).to_owned();
                let arg5 = outcome;
                let arg6 = (text).to_owned();
                self.store
                    .call("manager_control.finish_job", move |store| {
                        store.finish_job(&arg0, &arg1, &arg2, &arg3, &arg4, arg5, &arg6, now_ms())
                    })
                    .await
            }?;
            {
                let arg0 = (check.attempt_id).to_owned();
                self.store
                    .call("manager_control.resolve_pending_check", move |store| {
                        store.resolve_pending_check(&arg0)
                    })
                    .await
            }?;
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
        let job = {
            let arg0 = (check.conversation_id).to_owned();
            let arg1 = (check.trigger).to_owned();
            self.store
                .call("manager_control.scheduler_job", move |store| {
                    store.scheduler_job(&arg0, &arg1)
                })
                .await
        }
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
                !check
                    .process_groups
                    .iter()
                    .any(|root| vibeface_bridge::platform::tree::root_is_running(*root)),
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
        {
            let arg0 = (self.identity).to_owned();
            let arg1 = (network).to_owned();
            let arg2 = (check).to_owned();
            self.store
                .call("manager_control.confirm_manager_stopped", move |store| {
                    store.confirm_manager_stopped(&arg0, &arg1, &arg2, now_ms())
                })
                .await
        }?;
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
            .call("manager_control.pending_checks", move |store| {
                store.pending_checks()
            })
            .await?
            .into_iter()
            .find(|check| check.check_id == check_id && check.conversation_id == scope)
            .context("pending check not found")?;
        self.close_check(network, &check).await?;
        Ok(json!({"check_id":check_id,"resolved":true}))
    }

    /// Capability alone cannot vouch for a lost work journal, even while the target is online.
    pub(super) async fn local_work_vouches(
        &self,
        network: &str,
        conversation: &str,
        work_id: &str,
    ) -> Result<bool> {
        let kind = {
            let network = network.to_owned();
            let conversation = conversation.to_owned();
            let work_id = work_id.to_owned();
            let identity = self.identity.clone();
            self.store
                .call("manager_control.work_vouches", move |store| {
                    if store.pending_checks()?.iter().any(|check| {
                        check.conversation_id == conversation
                            && check.trigger == crate::scheduling::task_trigger(&work_id)
                    }) {
                        return Ok(None);
                    }
                    let Some(run) = store.work_record_in_scope(
                        &network,
                        &conversation,
                        &work_id,
                        &identity.device_id()?,
                    )?
                    else {
                        return Ok(None);
                    };
                    if matches!(run.state, WorkState::Unverified { .. }) {
                        return Ok(None);
                    }
                    Ok(Some(run.kind))
                })
                .await?
        };
        let Some(kind) = kind else {
            return Ok(false);
        };
        Ok(match kind {
            WorkKind::Agent(provider) if provider.on_app_server() => {
                self.codex.capability().await["connected"] == true
            }
            _ => true,
        })
    }

    async fn target_vouches(&self, network: &str, work: &WorkProjection) -> Result<bool> {
        if work.target_node == self.identity.device_id()? {
            self.local_work_vouches(network, &work.conversation_id, &work.work_id)
                .await
        } else {
            let request = json!({"type":"work_evidence",
                "conversation_id":work.conversation_id,"work_id":work.work_id});
            match tokio::time::timeout(
                std::time::Duration::from_secs(3),
                self.peer_request(network, &work.target_node, request),
            )
            .await
            {
                Ok(Ok(evidence)) => Ok(evidence["network_id"] == network
                    && evidence["conversation_id"] == work.conversation_id
                    && evidence["work_id"] == work.work_id
                    && evidence["vouches"] == true),
                _ => Ok(false),
            }
        }
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
        let work = {
            let arg0 = (network).to_owned();
            let arg1 = (scope).to_owned();
            self.store
                .call("manager_control.works", move |store| {
                    store.works(&arg0, &arg1)
                })
                .await
        }?
        .into_iter()
        .find(|work| work.work_id == work_id)
        .context("work not found in this conversation")?;
        match work.outcome {
            Some(WorkOutcome::Unverified) => {}
            Some(_) => bail!("这个结果已经不需要你确认了"),
            None => ensure!(
                !self.target_vouches(network, &work).await?,
                "它的设备仍持有原工作记录，可以继续报回结果。"
            ),
        }
        let unvouched = work.outcome.is_none();
        // An agent task's coordination process has its own check here; the work card answers both.
        let manager = self
            .store
            .call("manager_control.pending_checks", move |store| {
                store.pending_checks()
            })
            .await?
            .into_iter()
            .find(|check| check.trigger == crate::scheduling::task_trigger(work_id));
        let receipt = {
            let arg0 = (self.identity).to_owned();
            let arg1 = (network).to_owned();
            let arg2 = (scope).to_owned();
            let arg3 = (work_id).to_owned();
            let arg4 = outcome;
            let arg5 = (note).to_owned();
            let arg6 = super::attestation(input)?;
            let arg7 = unvouched;
            self.store
                .call("manager_control.settle_work", move |store| {
                    store.settle_work(
                        &arg0,
                        &arg1,
                        &arg2,
                        &arg3,
                        arg4,
                        &arg5,
                        arg6,
                        arg7,
                        now_ms(),
                    )
                })
                .await
        }?;
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

#[cfg(all(test, unix))]
pub(super) mod tests {
    use super::*;
    use crate::runtime::test_support::{access as test_access, manager_settings};

    pub(in crate::runtime) async fn pending_manager(
        node: &Runtime,
        admin: &crate::crypto::LocalIdentity,
        server: &str,
        thread: &str,
    ) -> Result<(Access, PendingCheck)> {
        super::super::triage::tests::bot(node, admin, "c", "fixture", &[]).await?;
        let (trigger, attempt) =
            super::super::triage::tests::start_reply(node, admin, "confirm-stop").await?;
        let network = node
            .store
            .call("manager_control.network", move |store| store.network())
            .await?
            .context("network")?;
        let identity = node.identity.clone();
        let server = server.to_owned();
        let thread = thread.to_owned();
        node.store
            .call("manager_control.pending_fixture", move |store| {
                let server = server.as_str();
                let thread = thread.as_str();
                store.record_manager_turn(
                    &attempt,
                    server,
                    thread,
                    Some("turn"),
                    &Default::default(),
                )?;
                store.finish_job(
                    &identity,
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
                Ok((test_access(network, true, Some("c".into())), check))
            })
            .await
    }

    #[tokio::test]
    async fn an_approval_lease_cannot_borrow_a_replacement_assignment() -> Result<()> {
        use crate::{
            model::EventBody,
            workspace::{AssignmentState, WorkspaceCommand},
        };
        let (node, admin, dir) = super::super::triage::tests::node().await?;
        let result = async {
            let root = dir.canonicalize()?;
            let first_root = root.join("first");
            let next_root = root.join("next");
            std::fs::create_dir_all(&first_root)?;
            std::fs::create_dir_all(&next_root)?;
            {
                let mut settings = node.settings.lock().unwrap();
                settings.roots = vec![root.clone()];
                settings.manager = Some(manager_settings(crate::providers::Provider::Codex, root, "fixture".into()));
            }
            super::super::triage::tests::bot(&node, &admin, "c", "Task Bot", &[]).await?;
            let network = node.store.call("manager_control.network", move |store| store.network()).await?.context("network")?;
            let local = node.identity.device_id()?;
            fn command(store: &mut crate::store::Store, identity: &crate::crypto::LocalIdentity,
                admin: &crate::crypto::LocalIdentity, network: &str, id: &str,
                command: WorkspaceCommand) -> Result<crate::workspace::WorkspaceCommandReceipt> {
                let signed = admin.attest(network, crate::workspace::WORKSPACE_SCOPE,
                    EventBody::WorkspaceCommanded { command_id: id.into(), command: command.clone(),
                        acting_assignment: None, attestation: None, cause: None })?;
                store.apply_workspace_command(identity, network, id, command,
                    signed.attestation().cloned(), None, None, now_ms())
            }
            let (job, first, attempt) = {
                let identity = node.identity.clone(); let boot = node.boot.clone(); let admin = admin.clone(); let network = network.clone(); let first_root = first_root.clone();
                node.store.call("manager_control.assignment_fixture", move |store| {
                command(store, &identity, &admin, &network, "create", serde_json::from_value(json!({
                    "kind":"create_task","task_id":"lease-task","owner_bot":"c","title":"lease",
                    "request":"lease","expected_result":"result","source_events":[],"dependencies":[],"reply_only":false
                }))?)?;
                let input = admin.attest(&network, "c", serde_json::from_value(json!({
                    "type":"input_posted","message_id":"lease-input","target":{"kind":"task","task_id":"lease-task"},
                    "intent":"continue","sequence":1,"expected_revision":1,"text":"run","report_results":true
                }))?)?;
                store.append_event(&identity, &network, "c", input, vec![], now_ms())?;
                let first = store.ensure_workspace_assignment(&identity, &network, "lease-task",
                    &[first_root.to_string_lossy().into_owned()], now_ms())?.context("first allocation")?;
                let job = store.claim_next_fixture(&identity, &network, "c", &boot, "lease-fence")?.context("Task claim")?.job;
                let attempt = job.state.attempt().context("attempt")?.id.clone();
                store.mark_started("c", &job.trigger, &attempt)?;
                store.started_workspace_assignment(&identity, &network, &job, now_ms())?;
                Ok((job, first, attempt))
                            }).await?
            };
            let _cancel = node.register_manager_attempt_fixture("lease-token".into(), test_access(network.clone(), false, Some("c".into())), &attempt, &job.trigger, crate::providers::Provider::Codex).await?;
            drop(node.manager_input_lease(&attempt, crate::providers::Provider::Codex).await?);
            {
                let identity = node.identity.clone(); let admin = admin.clone(); let network = network.clone(); let first = first.clone(); let local = local.clone(); let next_root = next_root.clone(); let job = job.clone();
                node.store.call("manager_control.replacement_fixture", move |store| {
                let proof = store.append_event(&identity, &network, "c", serde_json::from_value(json!({
                    "type":"message_posted","message_id":"shared-stop","role":"assistant","text":"stopped",
                    "in_reply_to":"lease-input","status":"stopped"
                }))?, vec![], now_ms())?;
                store.apply_workspace_command(&identity, &network, "stop-first", WorkspaceCommand::AssignmentProgress {
                    execution: None, assignment_id: first.assignment_id.clone(), revision: 1,
                    state: AssignmentState::Stopped, input_event: None, evidence_event: Some(proof.hash), result: None,
                }, None, None, None, now_ms())?;
                command(store, &identity, &admin, &network, "replace", WorkspaceCommand::AssignTask {
                    task_id: "lease-task".into(), expected_revision: 1, assignment_id: "lease-replacement".into(),
                    bot_id: "c".into(), execution_node: local.clone(), bot_owner_epoch: first.bot_owner_epoch,
                    channel_owner_epoch: None, work_id: None,
                    resource_roots: vec![next_root.to_string_lossy().into_owned()], automatic: false,
                })?;
                assert!(crate::store::workspace::task_input_allowed_from(&store.connection, &network, "lease-task")?);
                assert!(crate::store::workspace::assignment_resources_available_from(&store.connection, &network, &first)?);
                assert!(matches!(store.scheduler_job("c", &job.trigger)?.unwrap().state, JobState::Running { .. }));
            Ok(())
                }).await?
            }
            assert!(node.manager_input_lease(&attempt, crate::providers::Provider::Codex).await.is_err(),
                "a colocated approval must reject the still-running old manager before native response");
            Ok::<_, anyhow::Error>(())
        }.await;
        drop(node);
        std::fs::remove_dir_all(dir)?;
        result
    }
}
