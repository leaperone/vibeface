use super::*;
use crate::scheduling::SteerTarget;
use tokio::sync::{OwnedRwLockReadGuard, OwnedRwLockWriteGuard, RwLock};

impl Runtime {
    async fn work_native_directory(
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
        let store = self.store.lock().await;
        store.authorize_node(network, &local)?;
        ensure!(work.target == local, "work targets another Node");
        ensure!(
            store.work_native_write_allowed(network, &work.work_id)?,
            "work has no active authority for native submission"
        );
        if let WorkKind::Agent(provider) = work.kind {
            ensure!(
                provider == work.resolved.provider
                    && self.providers().switches.execution.contains(&provider),
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
            !store
                .stop_requests()?
                .iter()
                .any(|stop| stop.work_id == work.work_id)
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
        drop(store);
        if work.resolved.provider == Provider::Pi && work.thread.is_none() && thread.is_some() {
            self.work_native_directory(work.resolved.provider, thread, "")
                .await?;
        }
        self.work_native_directory(
            work.resolved.provider,
            thread.filter(|_| work.resolved.provider != Provider::Pi || work.thread.is_some()),
            &work.cwd,
        )
        .await?;
        let mut store = self.store.lock().await;
        store.authorize_node(network, &local)?;
        ensure!(
            self.local_permission_epoch
                .load(std::sync::atomic::Ordering::Acquire)
                == permission_epoch,
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
                self.providers().switches.execution.contains(&provider),
                "work execution permission was revoked"
            );
        }
        ensure!(
            store.reserve_work_native_submission(network, work)?,
            "work native submission was already reserved; query the original work"
        );
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
        let store = self.store.lock().await;
        store.authorize_node(network, &self.identity.device_id()?)?;
        ensure!(
            store
                .raw_work_steer_target(network, &self.identity.device_id()?, conversation, work_id)?
                .as_ref()
                == Some(into),
            "work input target is no longer active"
        );
        let work = store.work(network, work_id)?.context("work not found")?;
        drop(store);
        self.work_native_directory(work.resolved.provider, Some(&into.thread_id), &work.cwd)
            .await?;
        let store = self.store.lock().await;
        ensure!(
            self.providers().switches.execution.contains(&into.provider)
                && store
                    .raw_work_steer_target(
                        network,
                        &self.identity.device_id()?,
                        conversation,
                        work_id
                    )?
                    .as_ref()
                    == Some(into),
            "work input permission changed before native submission"
        );
        Ok(guard)
    }
}
