use super::*;
use crate::scheduling::SteerTarget;
use tokio::sync::{OwnedRwLockReadGuard, OwnedRwLockWriteGuard, RwLock};

impl Runtime {
    pub(crate) fn validate_assignment_roots(roots: &[String]) -> Result<()> {
        for root in roots {
            let path = PathBuf::from(root);
            let actual = path
                .canonicalize()
                .context("assignment resource directory does not exist")?;
            ensure!(
                path.is_absolute() && actual == path && actual.is_dir(),
                "assignment resource roots must name their actual canonical directories"
            );
        }
        Ok(())
    }

    pub(super) fn validate_work_assignment_roots(
        store: &Store,
        network: &str,
        trigger: &str,
    ) -> Result<()> {
        if let Some(id) = trigger.strip_prefix("assignment:") {
            let assignment = store
                .workspace_assignment(network, id)?
                .context("work assignment missing")?;
            Self::validate_assignment_roots(&assignment.resource_roots)?;
        }
        Ok(())
    }

    pub(super) async fn work_native_directory(
        &self,
        provider: Provider,
        thread: Option<&str>,
        cwd: &str,
    ) -> Result<PathBuf> {
        let actual = if let Some(thread) = thread {
            match provider {
                Provider::Codex => {
                    let reply = tokio::time::timeout(
                        Duration::from_secs(5),
                        self.codex.client().request(
                            "thread/read",
                            json!({"threadId":thread,"includeTurns":false}),
                        ),
                    )
                    .await
                    .context("native session directory query timed out")??;
                    Ok(PathBuf::from(
                        reply["thread"]["cwd"]
                            .as_str()
                            .context("native session has no verified directory")?,
                    ))
                }
                Provider::Claude => self.claude.native_directory(thread).await,
                Provider::Pi => self.pi.native_directory(thread).await,
                Provider::Opencode => self.opencode.native_directory(thread).await,
                Provider::Cursor => self.cursor_executor.native_directory(thread).await,
            }
            .context("native session directory could not be verified")?
        } else {
            PathBuf::from(cwd)
        };
        let actual = actual
            .canonicalize()
            .context("native work directory does not exist")?;
        if !cwd.is_empty() {
            ensure!(
                PathBuf::from(cwd).canonicalize()? == actual,
                "declared work directory differs from the native session"
            );
        }
        self.allowed_path(
            actual
                .to_str()
                .context("native work directory is not UTF-8")?,
        )
    }

    pub(super) async fn codex_native_execution(
        &self,
        network: &str,
        thread: &str,
        actual_cwd: &Path,
    ) -> Result<Option<crate::work::NativeExecution>> {
        let reply = tokio::time::timeout(
            Duration::from_secs(5),
            self.codex.client().request(
                "thread/read",
                json!({"threadId":thread,"includeTurns":true}),
            ),
        )
        .await
        .context("native execution query timed out")??;
        let native = &reply["thread"];
        let Some(turns) = native["turns"].as_array() else {
            return Ok(None);
        };
        ensure!(
            native["id"].as_str() == Some(thread),
            "native execution query returned another thread"
        );
        ensure!(
            PathBuf::from(
                native["cwd"]
                    .as_str()
                    .context("native session has no verified directory")?
            )
            .canonicalize()?
                == actual_cwd,
            "native session directory changed before submission"
        );
        let active: Vec<_> = turns
            .iter()
            .filter(|turn| turn["status"] == "inProgress")
            .collect();
        ensure!(
            active.len() <= 1,
            "native execution has ambiguous active turns"
        );
        let Some(turn) = active.first() else {
            return Ok(None);
        };
        let turn_id = turn["id"]
            .as_str()
            .context("native execution has no turn ID")?;
        vibeface_protocol::validate_id(turn_id).map_err(anyhow::Error::msg)?;
        let ids: Vec<String> = turn["items"]
            .as_array()
            .context("native execution query returned no items")?
            .iter()
            .filter(|item| item["type"] == "userMessage")
            .filter_map(|item| item["clientId"].as_str().map(str::to_owned))
            .collect();
        let call_network = network.to_owned();
        let mut work_ids = self
            .store
            .call("work_input.native_work_ids", move |store| {
                ids.into_iter()
                    .filter_map(|id| match store.work(&call_network, &id) {
                        Ok(Some(_)) => Some(Ok(id)),
                        Ok(None) => None,
                        Err(error) => Some(Err(error)),
                    })
                    .collect::<Result<Vec<_>>>()
            })
            .await?;
        work_ids.sort();
        work_ids.dedup();
        Ok(Some(crate::work::NativeExecution {
            thread_id: thread.into(),
            turn_id: turn_id.into(),
            work_ids,
        }))
    }

    pub(super) async fn work_start_lease(
        &self,
        network: &str,
        work: &crate::work::WorkSpec,
        thread: Option<&str>,
    ) -> Result<OwnedRwLockWriteGuard<()>> {
        let guard = self
            .work_input_lock(&work.work_id)
            .await
            .write_owned()
            .await;
        let local = self.identity.device_id()?;
        let permission_epoch = self
            .local_permission_epoch
            .load(std::sync::atomic::Ordering::Acquire);
        let call_network = network.to_owned();
        let call_work = work.clone();
        let call_thread = thread.map(str::to_owned);
        let call_local = local.clone();
        let write_state = self.write_state.clone();
        let providers = self.providers.clone();
        let request = self.store.call("work_input.start_admission", move |store| {
        let network = call_network.as_str(); let work = &call_work; let local = call_local;
        let thread = call_thread.as_deref();
        ensure!(*write_state.borrow() == WriteState::Writable, "Node execution authority is unavailable");
        store.authorize_node(network, &local)?;
        ensure!(work.target == local, "work targets another Node");
        ensure!(
            store.work_native_authority_allowed(network, &work.work_id)?,
            "work has no active authority for native submission"
        );
        Self::validate_work_assignment_roots(store, network, &work.trigger)?;
        if let WorkKind::Agent(provider) = work.kind {
            ensure!(
                provider == work.resolved.provider
                    && providers.lock().unwrap_or_else(std::sync::PoisonError::into_inner).switches.execution.contains(&provider),
                "work execution permission was revoked"
            );
        }
        let request = crate::work::WorkRequest {
            work_id: work.work_id.clone(),
            conversation: work.conversation.clone(),
            trigger: work.trigger.clone(),
            target: work.target.clone(),
            dispatcher: work.dispatcher.clone(),
            call_ref: work.call_ref.clone(),
            kind: match work.kind {
                WorkKind::Task => crate::work::RequestedWorkKind::Task,
                WorkKind::Agent(provider) => crate::work::RequestedWorkKind::Agent(Some(provider)),
            },
            model: work.model.clone(),
            cwd: work.cwd.clone(),
            title: work.title.clone(),
            prompt: work.prompt.clone(),
            continues: work.continues.clone(),
            parent: work.parent.clone(),
            thread: work.thread.clone(),
        };
        let admitted = store
            .accepted_work(network, &request)?
            .context("accepted work missing")?;
        ensure!(
            admitted.spec == *work
                && (matches!(admitted.record.state, crate::work::WorkState::Accepted)
                    || matches!(&admitted.record.state,crate::work::WorkState::Starting { thread:Some(thread) } if work.thread.as_ref()==Some(thread))),
            "work is no longer awaiting its first native submission"
        );
        ensure!(
            store.stop_request(&work.work_id)?.is_none()
                && !store.peer_cancelled(network, &work.conversation, &work.work_id)?,
            "work was cancelled before native submission"
        );
        ensure!(
            !store.work_native_submission_reserved(&work.work_id)?,
            "work native submission was already reserved; query the original work"
        );
        let expected_thread = match (&work.thread, &work.continues) {
            (Some(thread), None) => Some(thread.clone()),
            (None, Some(original)) => {
                let original = store
                    .work(network, original)?
                    .context("the continued work is not registered")?;
                ensure!(
                    original.target_node == local
                        && original.conversation_id == work.conversation
                        && original.kind == work.kind,
                    "the continued work belongs to another execution scope"
                );
                original.thread_id
            }
            (None, None) => None,
            (Some(_), Some(_)) => bail!("thread and continues are exclusive"),
        };
        ensure!(
            thread == expected_thread.as_deref(),
            "native session differs from the registered work"
        );
        Ok(request)
        }).await?;
        if work.resolved.provider == Provider::Pi && work.thread.is_none() && thread.is_some() {
            self.work_native_directory(work.resolved.provider, thread, "")
                .await?;
        }
        let actual_cwd = self
            .work_native_directory(
                work.resolved.provider,
                thread.filter(|_| work.resolved.provider != Provider::Pi || work.thread.is_some()),
                &work.cwd,
            )
            .await?;
        let execution = if work.resolved.provider == Provider::Codex {
            match work.thread.as_deref() {
                Some(thread) => {
                    self.codex_native_execution(network, thread, &actual_cwd)
                        .await?
                }
                None => None,
            }
        } else {
            None
        };
        let call_network = network.to_owned();
        let call_work = work.clone();
        let write_state = self.write_state.clone();
        let providers = self.providers.clone();
        let epoch = self.local_permission_epoch.clone();
        self.store
            .call("work_input.reserve_submission", move |store| {
                let network = call_network.as_str();
                let work = &call_work;
                ensure!(
                    *write_state.borrow() == WriteState::Writable,
                    "Node execution authority is unavailable"
                );
                store.authorize_node(network, &local)?;
                ensure!(
                    epoch.load(std::sync::atomic::Ordering::Acquire) == permission_epoch,
                    "work permissions changed before native submission"
                );
                ensure!(
                    store
                        .accepted_work(network, &request)?
                        .is_some_and(|admitted| admitted.spec == *work),
                    "accepted work changed before native submission"
                );
                if let WorkKind::Agent(provider) = work.kind {
                    ensure!(
                        providers
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .switches
                            .execution
                            .contains(&provider),
                        "work execution permission was revoked"
                    );
                }
                Self::validate_work_assignment_roots(store, network, &work.trigger)?;
                ensure!(
                    store.reserve_work_native_submission(
                        network,
                        work,
                        actual_cwd
                            .to_str()
                            .context("native work directory is not UTF-8")?,
                        execution.as_ref(),
                    )?,
                    "work native submission was already reserved; query the original work"
                );
                Ok(())
            })
            .await?;
        Ok(guard)
    }

    pub(super) async fn work_first_input_lease(
        &self,
        network: &str,
        work: &crate::work::WorkSpec,
        actual_session: &str,
    ) -> Result<OwnedRwLockWriteGuard<()>> {
        let guard = self
            .work_input_lock(&work.work_id)
            .await
            .write_owned()
            .await;
        let local = self.identity.device_id()?;
        let permission_epoch = self
            .local_permission_epoch
            .load(std::sync::atomic::Ordering::Acquire);
        ensure!(
            work.target == local
                && work.kind == WorkKind::Agent(work.resolved.provider)
                && self
                    .providers()
                    .switches
                    .execution
                    .contains(&work.resolved.provider),
            "work execution permission was revoked"
        );
        let call_network = network.to_owned();
        let call_work = work.clone();
        let call_local = local.clone();
        let write_state = self.write_state.clone();
        self.store
            .call("work_input.first_input_authority", move |store| {
                let network = call_network.as_str();
                let work = &call_work;
                let local = call_local;
                ensure!(
                    *write_state.borrow() == WriteState::Writable,
                    "Node execution authority is unavailable"
                );
                store.authorize_node(network, &local)?;
                ensure!(
                    store.work_native_authority_allowed(network, &work.work_id)?
                        && store.work_native_submission_reserved(&work.work_id)?,
                    "work no longer permits its first native input"
                );
                Self::validate_work_assignment_roots(store, network, &work.trigger)?;
                Ok(())
            })
            .await?;
        let actual_cwd = self
            .work_native_directory(work.resolved.provider, Some(actual_session), &work.cwd)
            .await?;
        let call_network = network.to_owned();
        let call_work = work.clone();
        let actual_session = actual_session.to_owned();
        let write_state = self.write_state.clone();
        let providers = self.providers.clone();
        let epoch = self.local_permission_epoch.clone();
        self.store
            .call("work_input.begin_first_input", move |store| {
                let network = call_network.as_str();
                let work = &call_work;
                ensure!(
                    *write_state.borrow() == WriteState::Writable,
                    "Node execution authority is unavailable"
                );
                store.authorize_node(network, &local)?;
                ensure!(
                    providers
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .switches
                        .execution
                        .contains(&work.resolved.provider)
                        && epoch.load(std::sync::atomic::Ordering::Acquire) == permission_epoch,
                    "work permissions changed before native input"
                );
                Self::validate_work_assignment_roots(store, network, &work.trigger)?;
                store.begin_reserved_work_native_input(
                    network,
                    work,
                    &actual_session,
                    actual_cwd
                        .to_str()
                        .context("native work directory is not UTF-8")?,
                )?;
                Ok(())
            })
            .await?;
        Ok(guard)
    }

    pub(super) async fn work_input_lock(&self, work_id: &str) -> Arc<RwLock<()>> {
        let mut locks = self.work_input_locks.lock().await;
        locks.retain(|_, lock| lock.strong_count() > 0);
        if let Some(lock) = locks.get(work_id).and_then(std::sync::Weak::upgrade) {
            return lock;
        }
        let lock = Arc::new(RwLock::new(()));
        locks.insert(work_id.into(), Arc::downgrade(&lock));
        lock
    }

    pub(super) async fn work_input_lease(
        &self,
        network: &str,
        conversation: &str,
        into: &SteerTarget,
    ) -> Result<OwnedRwLockReadGuard<()>> {
        let work_id = into
            .attempt_id
            .strip_prefix("work:")
            .context("invalid work input target")?;
        let guard = self.work_input_lock(work_id).await.read_owned().await;
        ensure!(
            self.providers().switches.execution.contains(&into.provider),
            "work execution permission was revoked"
        );
        let call_network = network.to_owned();
        let call_conversation = conversation.to_owned();
        let call_work = work_id.to_owned();
        let target = into.clone();
        let local = self.identity.device_id()?;
        let write_state = self.write_state.clone();
        let work = self
            .store
            .call("work_input.steer_authority", move |store| {
                let network = call_network.as_str();
                let conversation = call_conversation.as_str();
                let work_id = call_work.as_str();
                let into = &target;
                ensure!(
                    *write_state.borrow() == WriteState::Writable,
                    "Node execution authority is unavailable"
                );
                store.authorize_node(network, &local)?;
                ensure!(
                    store
                        .raw_work_steer_target(network, &local, conversation, work_id)?
                        .as_ref()
                        == Some(into),
                    "work input target is no longer active"
                );
                let work = store.work(network, work_id)?.context("work not found")?;
                Self::validate_work_assignment_roots(store, network, &work.trigger)?;
                Ok(work)
            })
            .await?;
        let actual_cwd = self
            .work_native_directory(work.resolved.provider, Some(&into.thread_id), &work.cwd)
            .await?;
        let call_network = network.to_owned();
        let call_conversation = conversation.to_owned();
        let call_work = work_id.to_owned();
        let target = into.clone();
        let local = self.identity.device_id()?;
        let write_state = self.write_state.clone();
        let providers = self.providers.clone();
        self.store
            .call("work_input.steer_admission", move |store| {
                let network = call_network.as_str();
                let conversation = call_conversation.as_str();
                let work_id = call_work.as_str();
                let into = &target;
                ensure!(
                    *write_state.borrow() == WriteState::Writable,
                    "Node execution authority is unavailable"
                );
                Self::validate_work_assignment_roots(store, network, &work.trigger)?;
                ensure!(
                    providers
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .switches
                        .execution
                        .contains(&into.provider)
                        && store
                            .raw_work_steer_target(network, &local, conversation, work_id)?
                            .as_ref()
                            == Some(into)
                        && store.work_native_resources_available(
                            network,
                            work_id,
                            actual_cwd
                                .to_str()
                                .context("native work directory is not UTF-8")?,
                        )?,
                    "work input permission changed before native submission"
                );
                Ok(())
            })
            .await?;
        Ok(guard)
    }
}
