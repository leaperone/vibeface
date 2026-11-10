use super::{Changed, NodeSettings, Runtime, manager_control::ActiveManager, now_ms};
use crate::{
    model::WorkKind,
    providers::{Local, Provider},
    scheduling::{JobState, task_trigger},
    work::{Handle, Probe, StopRequest, WorkRecord, WorkState},
};
use anyhow::{Context, Result};
use futures_util::StreamExt;
use std::{
    collections::{BTreeSet, HashMap},
    path::{Path, PathBuf},
    sync::{Arc, atomic::Ordering},
    time::Duration,
};

impl Runtime {
    fn execution_fence_works(store: &crate::store::Store, local: &str) -> Result<Vec<WorkRecord>> {
        let Some(network) = store.network()? else {
            return Ok(Vec::new());
        };
        let mut owned = Vec::new();
        for record in store.work_records()? {
            if record.state.finished() || !matches!(record.kind, WorkKind::Agent(_)) {
                continue;
            }
            if store.work(&network, &record.work_id)?.is_some_and(|work| {
                work.target_node == local
                    && work.conversation_id == record.conversation
                    && work.kind == record.kind
            }) {
                owned.push(record);
            }
        }
        Ok(owned)
    }

    /// Only local stop intent is stored here. Shared events are forbidden after a fork.
    pub(super) fn hold_execution_fence_stops(store: &mut crate::store::Store, local: &str) -> Result<()> {
        let at = now_ms();
        let mut stops: BTreeSet<_> = store
            .stop_requests()?
            .into_iter()
            .map(|request| request.work_id)
            .collect();
        for record in Self::execution_fence_works(store, local)? {
            if stops.insert(record.work_id.clone()) {
                store.save_stop_request(&StopRequest {
                    work_id: record.work_id,
                    conversation: record.conversation,
                    target: local.to_owned(),
                    requested_at_ms: at,
                    deadline_ms: at + super::work::STOP_DELIVERY_TIMEOUT.as_millis() as i64,
                    timed_out: false,
                })?;
            }
        }
        Ok(())
    }

    /// Read and interrupt owned work without publishing results or replaying submissions.
    /// Provider waits never hold Store, manager controls, or input admission locks.
    pub(super) async fn reconcile_execution_fence(&self) -> Result<()> {
        let local = self.identity.device_id()?;
        let records = self.store.call("execution_fence", move |store| {
            if let Err(error) = Self::hold_execution_fence_stops(store, &local) {
                tracing::warn!(%error, "execution fence could not retain local stop intent");
            }
            Self::execution_fence_works(store, &local)
        }).await?;
        futures_util::stream::iter(records)
        .for_each_concurrent(8, |record| async move {
            if let Err(error) = self.stop_fenced_work(record).await {
                tracing::debug!(%error, "execution fence stop awaits provider confirmation");
            }
        })
        .await;
        // Receipts remain queryable, but cannot become queued inputs on this copy.
        let jobs = self.store.call("fenced_steering_jobs", |store| store.steering_jobs()).await?;
        for job in jobs {
            if let JobState::Steering { into, .. } = &job.state {
                let _ = tokio::time::timeout(
                    Duration::from_secs(5),
                    self.executor(into.provider)?.read_back(into, &job.reply_to),
                )
                .await;
            }
        }
        Ok(())
    }

    async fn stop_fenced_work(&self, mut record: WorkRecord) -> Result<()> {
        let at = now_ms();
        {
            let mut retries = self.stop_retries.lock().expect("stop retries");
            let (attempts, next) = retries.entry(record.work_id.clone()).or_insert((0, at));
            if at < *next {
                return Ok(());
            }
            *next = at + (3_000_i64 << (*attempts).min(5)).min(60_000);
            *attempts = attempts.saturating_add(1);
        }
        let executor = self.work_executor(&record)?;
        let within = Duration::from_secs(5);
        if record.state.thread().is_some() {
            match tokio::time::timeout(within, executor.probe(&record)).await {
                Ok(Ok(Probe::Running { turn_id, epoch })) => {
                    record.state = WorkState::Running {
                        handle: Handle {
                            thread_id: record.state.thread().context("fenced work thread")?.into(),
                            turn_id,
                            epoch,
                        },
                    };
                }
                Ok(Ok(Probe::Terminal { .. } | Probe::NotStarted | Probe::Missing)) => {
                    return Ok(());
                }
                // With a known handle, a failed read must not prevent a scoped stop.
                _ if record.state.handle().is_some() => {}
                _ => return Ok(()),
            }
        } else if record.kind != WorkKind::Agent(Provider::Pi) {
            return Ok(());
        }
        tokio::time::timeout(within, executor.interrupt(&record))
            .await
            .context("execution fence interrupt timed out")??;
        // Acknowledgement is not completion. Keep the durable stop and original work record.
        Ok(())
    }

    /// The caller holds `config_save` and `manager_controls`; native cancellation drains later.
    pub(super) async fn fence_revoked_permissions(
        &self,
        active: &HashMap<String, Arc<ActiveManager>>,
        previous: &NodeSettings,
        next: &NodeSettings,
        previous_providers: &Local,
        next_providers: &Local,
    ) -> Result<()> {
        let canonical_roots = |roots: &[PathBuf]| {
            roots
                .iter()
                .map(|root| root.canonicalize().unwrap_or_else(|_| root.clone()))
                .collect::<Vec<_>>()
        };
        let previous_roots = canonical_roots(&previous.roots);
        let next_roots = canonical_roots(&next.roots);
        let roots_narrowed = previous_roots
            .iter()
            .any(|root| !next_roots.iter().any(|allowed| root.starts_with(allowed)));
        let manager_disabled = previous.manager.is_some() && next.manager.is_none();
        let execution: BTreeSet<Provider> = previous_providers
            .switches
            .execution
            .difference(&next_providers.switches.execution)
            .copied()
            .collect();
        if !roots_narrowed && !manager_disabled && execution.is_empty() {
            return Ok(());
        }
        let epoch = self
            .local_permission_epoch
            .load(Ordering::Acquire)
            .checked_add(1)
            .context("local permission epoch overflow")?;
        self.local_permission_epoch.store(epoch, Ordering::Release);
        let management_revoked = roots_narrowed || manager_disabled;
        if management_revoked {
            // Every management prompt captured the Node's roots, so it retains a removed grant.
            for invocation in active.values() {
                invocation.fenced.store(true, Ordering::Release);
                invocation.cancel.send_replace(true);
            }
        }
        let identity = self.identity.clone();
        let changes = self.store.call("fence_revoked_permissions", move |store| {
        let Some(network) = store.network()? else {
            return Ok(None);
        };
        let mut changed_jobs = Vec::new();
        let mut changed_works = Vec::new();
        if management_revoked {
            for job in store.scheduler_jobs_all()? {
                if matches!(
                    job.state,
                    JobState::Claimed { .. } | JobState::Running { .. }
                ) {
                    store.request_manager_cancel(
                        &identity,
                        &network,
                        &job.conversation_id,
                        &job.trigger,
                    )?;
                    changed_jobs.push((job.conversation_id, job.trigger));
                }
            }
        }
        let local = identity.device_id()?;
        let at = now_ms();
        let mut stops: BTreeSet<_> = store
            .stop_requests()?
            .into_iter()
            .map(|request| request.work_id)
            .collect();
        for record in store.work_records()? {
            if record.state.finished() {
                continue;
            }
            let work = store
                .work(&network, &record.work_id)?
                .context("active work permission scope missing")?;
            let cwd = Path::new(&work.cwd).canonicalize();
            // An inaccessible cwd cannot prove that its directory grant was retained.
            let removed_root = roots_narrowed
                && cwd.as_ref().map_or(true, |cwd| {
                    previous_roots.iter().any(|root| cwd.starts_with(root))
                        && !next_roots.iter().any(|root| cwd.starts_with(root))
                });
            match record.kind {
                WorkKind::Task if manager_disabled || removed_root => {
                    let trigger = task_trigger(&record.work_id);
                    store.request_manager_cancel(
                        &identity,
                        &network,
                        &record.conversation,
                        &trigger,
                    )?;
                    changed_jobs.push((record.conversation.clone(), trigger));
                }
                WorkKind::Agent(provider)
                    if (removed_root || execution.contains(&provider))
                        && stops.insert(record.work_id.clone()) =>
                {
                    store.save_stop_request(&StopRequest {
                        work_id: record.work_id.clone(),
                        conversation: record.conversation.clone(),
                        target: local.clone(),
                        requested_at_ms: at,
                        deadline_ms: at + super::work::STOP_DELIVERY_TIMEOUT.as_millis() as i64,
                        timed_out: false,
                    })?;
                }
                _ => continue,
            }
            changed_works.push((record.conversation, record.work_id));
        }
        Ok(Some((network, changed_jobs, changed_works)))
        }).await?;
        if let Some((network, jobs, works)) = changes {
            for (scope, trigger) in jobs {
                self.publish_changed(&network, &scope, Changed::Job(&trigger));
            }
            for (scope, work) in works {
                self.publish_changed(&network, &scope, Changed::Work(&work));
            }
        }
        if management_revoked {
            let mut tokens = self.agent_tokens.lock().await;
            for invocation in active.values() {
                tokens.remove(&invocation.token);
            }
        }
        self.outbox_wakeup.notify_one();
        self.schedule_wakeup.notify_one();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::test_support::{access as test_access, manager_settings, provider_work};
    use crate::{
        crypto::LocalIdentity,
        runtime::{
            Access,
            triage::tests::{bot, node, start_reply},
        },
        scheduling::SteerTarget,
        work::{Signal, WorkSpec, WorkState},
    };
    use serde_json::{Value, json};
    use std::time::Duration;
    use tokio::sync::watch;

    async fn native_directories(node: &Runtime, directories: &[(&str, &Path)]) -> Result<()> {
        let directories = directories
            .iter()
            .map(|(thread, directory)| Ok((thread.to_string(), directory.canonicalize()?)))
            .collect::<Result<std::collections::BTreeMap<_, _>>>()?;
        super::super::triage::tests::scripted_app_server(node, move |request| {
            if request["method"] == "thread/read" {
                return match request["params"]["threadId"]
                    .as_str()
                    .and_then(|thread| directories.get(thread))
                {
                    Some(cwd) => json!({"result":{"thread":{"cwd":cwd,"turns":[]}}}),
                    None => json!({"error":{"code":-32602,"message":"unknown fixture thread"}}),
                };
            }
            json!({"result":{}})
        })
        .await?;
        tokio::time::timeout(Duration::from_secs(5), async {
            while node.codex.status().await["connection"]["state"] != "connected" {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await?;
        Ok(())
    }

    async fn active_turn(
        node: &Runtime,
        admin: &LocalIdentity,
        directory: &Path,
    ) -> Result<(String, String, Access, watch::Receiver<bool>)> {
        let directory = directory.canonicalize()?;
        {
            let mut settings = node.settings.lock().unwrap();
            settings.roots = vec![directory.clone()];
            settings.manager = Some(manager_settings(
                Provider::Codex,
                directory,
                "fixture".into(),
            ));
        }
        {
            let mut providers = node.providers.lock().unwrap();
            providers.switches.execution.insert(Provider::Pi);
            providers
                .bins
                .insert(Provider::Pi, PathBuf::from("/fixture/pi"));
        }
        bot(node, admin, "c", "Ops", &[]).await?;
        let network = node.store.call("fixture_network", |store| store.network()).await?.context("network")?;
        let (trigger, attempt) = start_reply(node, admin, "m1").await?;
        let cancel = node
            .register_manager_attempt_fixture(
                "permission-token".into(),
                test_access(network, false, Some("c".into())),
                &attempt,
                &trigger,
                Provider::Codex,
            )
            .await?;
        let access = node
            .agent_tokens
            .lock()
            .await
            .get("permission-token")
            .cloned()
            .context("active tool token")?;
        Ok((trigger, attempt, access, cancel))
    }

    async fn running_work(
        node: &Runtime,
        network: &str,
        cwd: &Path,
        work_id: &str,
        provider: Provider,
    ) -> Result<SteerTarget> {
        let local = node.identity.device_id()?;
        let target = SteerTarget {
            provider,
            attempt_id: format!("work:{work_id}"),
            thread_id: format!("thread-{work_id}"),
            turn_id: format!("turn-{work_id}"),
        };
        if provider == Provider::Pi {
            let directory = node.data_dir.join("pi");
            crate::config::private_dir(&directory)?;
            let transcript = directory.join(format!("{}.jsonl", target.thread_id));
            crate::config::private_write(&transcript, b"")?;
            crate::config::private_write(
                &directory.join(format!("{}.json", target.thread_id)),
                &serde_json::to_vec(&json!({"file":transcript,"cwd":cwd.canonicalize()?,
                    "tree":"0","after":Value::Null}))?,
            )?;
        }
        let spec = WorkSpec {
            work_id: work_id.into(),
            call_ref: work_id.into(),
            cwd: cwd.to_string_lossy().into_owned(),
            title: work_id.into(),
            thread: Some(target.thread_id.clone()),
            ..provider_work(provider, &local, cwd, "work")
        };
        let identity = node.identity.clone();
        let network = network.to_owned();
        let work_id = work_id.to_owned();
        let actual_cwd = cwd.canonicalize()?.to_str().context("fixture cwd")?.to_owned();
        let fixture_target = target.clone();
        let epoch = node.codex.epoch();
        node.store.call("fixture_running_work", move |store| {
        store.accept_work_fixture(&identity, &network, spec.clone(), now_ms())?;
        store.reserve_work_native_submission(&network, &spec, &actual_cwd, None)?;
        store.begin_reserved_work_native_input(&network, &spec, &fixture_target.thread_id, &actual_cwd)?;
        store.apply_work_signal(
            &identity,
            &network,
            &work_id,
            Signal::TurnStarted {
                turn_id: fixture_target.turn_id.clone(),
                epoch,
            },
            now_ms(),
        )?;
            Ok(())
        }).await?;
        Ok(target)
    }

    #[tokio::test]
    async fn lost_write_authority_cancels_managers_and_refuses_existing_and_new_inputs() -> Result<()> {
        for state in [crate::store::WriteState::Forked, crate::store::WriteState::ReadOnly] {
            let (node, admin, directory) = node().await?;
            let (trigger, attempt, access, cancel) = active_turn(&node, &admin, &directory).await?;
            let network = &access.network_id;
            let target = running_work(&node, network, &directory, "running", Provider::Codex).await?;
            native_directories(&node, &[("thread-running", &directory)]).await?;
            let held_manager = node.manager_input_lease(&attempt, Provider::Codex).await?;
            let held_work = node.work_input_lease(network, "c", &target).await?;
            let queued = WorkSpec {
                work_id: "queued".into(), call_ref: "queued".into(),
                ..provider_work(Provider::Codex, &node.identity.device_id()?, &directory, "queued")
            };
            let before = {
                let identity = node.identity.clone(); let network = network.clone(); let queued = queued.clone();
                node.store.call("fixture_queued_work", move |store| {
                    store.accept_work_fixture(&identity, &network, queued, now_ms())?;
                    store.version_vector(&network)
                }).await?
            };
            assert!(tokio::time::timeout(Duration::from_secs(1),
                node.set_write_state(network, &[crate::store::WriteState::Writable], state)).await?);
            assert!(*cancel.borrow());
            assert!(node.manager_input_lease(&attempt, Provider::Codex).await.is_err());
            assert!(node.manager_tool_lease(&access).await.is_err());
            assert!(node.work_input_lease(network, "c", &target).await.is_err());
            assert!(node.work_start_lease(network, &queued, None).await.is_err());
            assert!(node.work_first_input_lease(network, &queued, "queued-thread").await.is_err());
            assert!(node.register_manager_attempt_fixture("late".into(), access.clone(),
                &attempt, &trigger, Provider::Codex).await.is_err());
            {
                let network = network.clone(); let trigger = trigger.clone();
                node.store.call("fixture_fenced_state", move |store| {
                assert_eq!(store.version_vector(&network)?, before);
                assert!(matches!(store.scheduler_job("c", &trigger)?.context("job")?.state,
                    JobState::Running { .. }));
                assert!(matches!(store.work_record("queued")?.context("queued work")?.state,
                    WorkState::Accepted));
                assert_eq!(store.stop_requests()?.len(), 2);
                Ok(())
                }).await?;
            }
            drop(held_work);
            drop(held_manager);
            if state == crate::store::WriteState::ReadOnly {
                assert!(node.set_write_state(network, &[state], crate::store::WriteState::Writable).await);
                assert!(node.manager_tool_lease(&access).await.is_err());
                assert!(node.work_start_lease(network, &queued, None).await.is_err(),
                    "restoring write authority cannot replay fenced work");
            }
            drop(node);
            std::fs::remove_dir_all(directory)?;
        }
        Ok(())
    }

    #[tokio::test]
    async fn write_authority_loss_at_maximum_epoch_cannot_restore_old_permissions() -> Result<()> {
        let (node, admin, directory) = node().await?;
        let (_, _, access, cancel) = active_turn(&node, &admin, &directory).await?;
        node.local_permission_epoch.store(u64::MAX, Ordering::Release);
        assert!(node.set_write_state(&access.network_id, &[crate::store::WriteState::Writable],
            crate::store::WriteState::ReadOnly).await);
        assert!(*cancel.borrow());
        assert!(!node.set_write_state(&access.network_id, &[crate::store::WriteState::ReadOnly],
            crate::store::WriteState::Writable).await);
        assert!(!node.writable());
        assert!(node.manager_tool_lease(&access).await.is_err());
        drop(node);
        std::fs::remove_dir_all(directory)?;
        Ok(())
    }

    #[tokio::test]
    async fn cancelled_permission_revocation_still_delivers_manager_cancellation() -> Result<()> {
        let (node, admin, directory) = node().await?;
        let (_, attempt, access, cancel) = active_turn(&node, &admin, &directory).await?;
        let previous = node.settings_snapshot();
        let mut next = previous.clone();
        next.manager = None;
        let providers = node.providers();
        let (entered, held) = tokio::sync::oneshot::channel();
        let (release, waiting) = std::sync::mpsc::channel();
        let held_node = node.clone();
        let holding = tokio::spawn(async move {
            held_node.store.call("fixture_hold_writer", move |_| {
                let _ = entered.send(()); waiting.recv()?; Ok(())
            }).await
        });
        held.await?;
        let task_node = node.clone();
        let task = tokio::spawn(async move {
            let controls = task_node.manager_controls.lock().await;
            task_node.fence_revoked_permissions(&controls, &previous, &next,
                &providers, &providers).await
        });
        tokio::time::timeout(Duration::from_secs(1), async {
            while !*cancel.borrow() { tokio::task::yield_now().await; }
        }).await?;
        task.abort();
        assert!(task.await.expect_err("cancelled revocation").is_cancelled());
        release.send(())?;
        holding.await??;
        assert!(node.manager_tool_lease(&access).await.is_err());
        assert!(node.manager_input_lease(&attempt, Provider::Codex).await.is_err());
        drop(node);
        std::fs::remove_dir_all(directory)?;
        Ok(())
    }

    #[tokio::test]
    async fn forked_scheduler_interrupts_owned_turn_then_only_reads_without_replay() -> Result<()> {
        let (node, admin, directory) = node().await?;
        let (_, _, access, cancel) = active_turn(&node, &admin, &directory).await?;
        let network = &access.network_id;
        running_work(&node, network, &directory, "owned", Provider::Codex).await?;
        let requests = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let stopped = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let calls = requests.clone();
        let ended = stopped.clone();
        super::super::triage::tests::scripted_app_server(&node, move |request| {
            let method = request["method"].as_str().unwrap_or("");
            match method {
                "thread/read" => {
                    calls.lock().unwrap().push(method.into());
                    assert_eq!(request["params"]["threadId"], "thread-owned");
                    json!({"result":{"thread":{"id":"thread-owned","turns":[{
                        "id":"turn-owned", "status":if ended.load(Ordering::Acquire) {
                            "interrupted"
                        } else { "inProgress" }, "items":[]
                    }]}}})
                }
                "turn/interrupt" => {
                    calls.lock().unwrap().push(method.into());
                    assert_eq!(request["params"]["threadId"], "thread-owned");
                    assert_eq!(request["params"]["turnId"], "turn-owned");
                    ended.store(true, Ordering::Release);
                    json!({"result":{}})
                }
                _ => {
                    assert!(!method.starts_with("turn/"), "a fork cannot submit or replay input");
                    json!({"result":{}})
                }
            }
        }).await?;
        tokio::time::timeout(Duration::from_secs(5), async {
            while node.codex.status().await["connection"]["state"] != "connected" {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        }).await?;
        let before = { let network = network.clone(); node.store.call("fixture_vector", move |store| store.version_vector(&network)).await? };
        assert!(node.set_write_state(network, &[crate::store::WriteState::Writable],
            crate::store::WriteState::Forked).await);
        assert!(*cancel.borrow());
        node.schedule_tick().await?;
        assert!(stopped.load(Ordering::Acquire));
        node.stop_retries.lock().unwrap().clear();
        node.schedule_tick().await?;
        assert_eq!(*requests.lock().unwrap(), vec!["thread/read", "turn/interrupt", "thread/read"]);
        let query_network = network.clone();
        node.store.call("fixture_forked_state", move |store| {
        assert_eq!(store.version_vector(&query_network)?, before);
        assert!(matches!(store.work_record("owned")?.context("work")?.state, WorkState::Running { .. }),
            "readback must not fabricate a shared completion after writes close");
        assert_eq!(store.stop_requests()?.len(), 1);
        Ok(())
        }).await?;
        drop(node);
        std::fs::remove_dir_all(directory)?;
        Ok(())
    }

    #[test]
    fn assignment_roots_reject_two_aliases_of_one_physical_directory() -> Result<()> {
        let directory =
            std::env::temp_dir().join(format!("vibeface-canonical-roots-{}", uuid::Uuid::new_v4()));
        let actual = directory.join("actual");
        std::fs::create_dir_all(&actual)?;
        let actual = actual.canonicalize()?;
        Runtime::validate_assignment_roots(&[actual.to_string_lossy().into_owned()])?;
        for name in ["first-alias", "second-alias"] {
            let alias = directory.join(name);
            vibeface_bridge::platform::privatefs::link_dir(&actual, &alias)?;
            let signed_roots = vec![alias.to_string_lossy().into_owned()];
            let error = Runtime::validate_assignment_roots(&signed_roots)
                .expect_err("aliases cannot lock distinct physical resources");
            assert!(error.to_string().contains("actual canonical directories"));
            assert_eq!(signed_roots[0], alias.to_string_lossy());
        }
        std::fs::remove_dir_all(directory)?;
        Ok(())
    }

    #[tokio::test]
    async fn permission_epoch_overflow_does_not_wrap_or_commit_revocation() -> Result<()> {
        let (node, admin, directory) = node().await?;
        let (_, _, _, cancel) = active_turn(&node, &admin, &directory).await?;
        let previous = node.settings_snapshot();
        let retained = directory.join("retained");
        std::fs::create_dir(&retained)?;
        let mut next = previous.clone();
        next.roots = vec![retained];
        node.local_permission_epoch
            .store(u64::MAX, Ordering::Release);
        let error = node
            .configure_node(next, &admin.device_id()?, None)
            .await
            .expect_err("revocation cannot wrap the permission epoch");
        assert!(format!("{error:#}").contains("local permission epoch overflow"));
        assert_eq!(
            node.local_permission_epoch.load(Ordering::Acquire),
            u64::MAX
        );
        assert_eq!(node.settings_snapshot().roots, previous.roots);
        assert!(!*cancel.borrow());
        drop(node);
        std::fs::remove_dir_all(directory)?;
        Ok(())
    }

    #[tokio::test]
    async fn ordinary_model_and_provider_changes_keep_the_captured_turn() -> Result<()> {
        let (node, admin, directory) = node().await?;
        let (trigger, attempt, access, cancel) = active_turn(&node, &admin, &directory).await?;
        let epoch = node.local_permission_epoch.load(Ordering::Acquire);
        let save = node.config_save.lock().await;
        node.write_providers(
            |target| {
                target.manager.as_mut().context("manager")?.model = "next-model".into();
                Ok(())
            },
            None,
        )
        .await?;
        drop(node.manager_input_lease(&attempt, Provider::Codex).await?);
        node.write_providers(
            |target| {
                target.manager.as_mut().context("manager")?.provider = Provider::Pi;
                Ok(())
            },
            None,
        )
        .await?;
        drop(save);
        assert_eq!(node.local_permission_epoch.load(Ordering::Acquire), epoch);
        assert!(!*cancel.borrow());
        assert_eq!(node.attempt_provider(&attempt).await, Some(Provider::Codex));
        drop(node.manager_input_lease(&attempt, Provider::Codex).await?);
        assert!(
            node.manager_input_lease(&attempt, Provider::Pi)
                .await
                .is_err()
        );
        drop(node.manager_tool_lease(&access).await?);
        assert!(matches!(
            { let trigger = trigger.clone();
                node.store.call("fixture_scheduler_job", move |store| store.scheduler_job("c", &trigger)).await? }
                .context("job")?
                .state,
            JobState::Running { .. }
        ));
        drop(node);
        std::fs::remove_dir_all(directory)?;
        Ok(())
    }

    #[tokio::test]
    async fn narrowed_roots_reject_new_inputs_and_tools_without_waiting_for_old_inputs()
    -> Result<()> {
        let (node, admin, directory) = node().await?;
        let (trigger, attempt, access, cancel) = active_turn(&node, &admin, &directory).await?;
        let retained = directory.join("retained");
        std::fs::create_dir(&retained)?;
        let retained = retained.canonicalize()?;
        let retained_alias = directory.join("retained-alias");
        vibeface_bridge::platform::privatefs::link_dir(&retained, &retained_alias)?;
        let removed = directory.join("removed");
        std::fs::create_dir(&removed)?;
        let removed = removed.canonicalize()?;
        let network = &access.network_id;
        let target = running_work(&node, network, &removed, "removed", Provider::Codex).await?;
        let kept =
            running_work(&node, network, &retained_alias, "retained", Provider::Codex).await?;
        native_directories(
            &node,
            &[
                ("thread-removed", &removed),
                ("thread-retained", &retained_alias),
            ],
        )
        .await?;
        let held = node.manager_input_lease(&attempt, Provider::Codex).await?;
        let held_work = node.work_input_lease(network, "c", &target).await?;
        let mut settings = node.settings_snapshot();
        settings.roots = vec![retained];
        tokio::time::timeout(
            Duration::from_secs(1),
            node.configure_node(settings, &admin.device_id()?, None),
        )
        .await??;
        assert!(*cancel.borrow());
        assert_eq!(node.local_permission_epoch.load(Ordering::Acquire), 1);
        assert!(
            node.manager_input_lease(&attempt, Provider::Codex)
                .await
                .is_err()
        );
        assert!(node.manager_tool_lease(&access).await.is_err());
        assert!(node.work_input_lease(network, "c", &target).await.is_err());
        drop(node.work_input_lease(network, "c", &kept).await?);
        {
            let trigger = trigger.clone();
            node.store.call("fixture_revoked_state", move |store| {
            assert!(matches!(
                store.scheduler_job("c", &trigger)?.context("job")?.state,
                JobState::Cancelling { .. }
            ));
            let stops = store.stop_requests()?;
            assert_eq!(stops.len(), 1);
            assert_eq!(stops[0].work_id, "removed");
            assert!(matches!(
                store.work_record("removed")?.context("work")?.state,
                WorkState::Running { .. }
            ));
            Ok(())
            }).await?;
        }
        let mut restored = node.settings_snapshot();
        restored.roots = vec![directory.canonicalize()?];
        node.configure_node(restored, &admin.device_id()?, None)
            .await?;
        assert_eq!(node.local_permission_epoch.load(Ordering::Acquire), 1);
        assert!(node.work_input_lease(network, "c", &target).await.is_err());
        let stale = node
            .register_manager_attempt(
                "stale-prepared".into(),
                access.clone(),
                &attempt,
                &trigger,
                Provider::Codex,
                0,
            )
            .await
            .expect_err("restoring a root cannot restore a revoked prepared attempt");
        assert!(stale.to_string().contains("permissions changed"));
        drop(held_work);
        drop(held);
        drop(node);
        std::fs::remove_dir_all(directory)?;
        Ok(())
    }

    #[tokio::test]
    async fn execution_revocation_stops_only_its_work_and_manager_revocation_cancels_the_bot()
    -> Result<()> {
        let (node, admin, directory) = node().await?;
        let (trigger, attempt, access, cancel) = active_turn(&node, &admin, &directory).await?;
        let network = &access.network_id;
        let codex_directory = directory.join("codex-work");
        let pi_directory = directory.join("pi-work");
        std::fs::create_dir(&codex_directory)?;
        std::fs::create_dir(&pi_directory)?;
        let target = running_work(
            &node,
            network,
            &codex_directory,
            "codex-work",
            Provider::Codex,
        )
        .await?;
        let other = running_work(&node, network, &pi_directory, "pi-work", Provider::Pi).await?;
        let save = node.config_save.lock().await;
        node.write_providers(
            |target| {
                target.local.switches.execution.remove(&Provider::Codex);
                Ok(())
            },
            None,
        )
        .await?;
        assert!(!*cancel.borrow());
        drop(node.manager_input_lease(&attempt, Provider::Codex).await?);
        assert!(node.work_input_lease(network, "c", &target).await.is_err());
        drop(node.work_input_lease(network, "c", &other).await?);
        node.write_providers(
            |target| {
                *target.manager = None;
                Ok(())
            },
            None,
        )
        .await?;
        drop(save);
        assert!(*cancel.borrow());
        assert!(node.manager_tool_lease(&access).await.is_err());
        node.store.call("fixture_revoked_provider", move |store| {
        assert!(matches!(
            store.scheduler_job("c", &trigger)?.context("job")?.state,
            JobState::Cancelling { .. }
        ));
        assert!(matches!(
            store.work_record("codex-work")?.context("work")?.state,
            WorkState::Running { .. }
        ));
        assert_eq!(
            store
                .stop_requests()?
                .iter()
                .map(|stop| stop.work_id.as_str())
                .collect::<Vec<_>>(),
            vec!["codex-work"]
        );
        Ok(())
        }).await?;
        drop(node);
        std::fs::remove_dir_all(directory)?;
        Ok(())
    }
}
