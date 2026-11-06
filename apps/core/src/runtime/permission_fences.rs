use super::{Changed, NodeSettings, Runtime, manager_control::ActiveManager, now_ms};
use crate::{
    model::WorkKind,
    providers::{Local, Provider},
    scheduling::{JobState, task_trigger},
    work::StopRequest,
};
use anyhow::{Context, Result};
use std::{
    collections::{BTreeSet, HashMap},
    path::{Path, PathBuf},
    sync::{Arc, atomic::Ordering},
};

impl Runtime {
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
        self.local_permission_epoch
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |epoch| {
                epoch.checked_add(1)
            })
            .map_err(|_| anyhow::anyhow!("local permission epoch overflow"))?;
        let management_revoked = roots_narrowed || manager_disabled;
        if management_revoked {
            // Every management prompt captured the Node's roots, so it retains a removed grant.
            for invocation in active.values() {
                invocation.fenced.store(true, Ordering::Release);
            }
        }
        let mut store = self.store.lock().await;
        let Some(network) = store.network()? else {
            return Ok(());
        };
        if management_revoked {
            for job in store.scheduler_jobs_all()? {
                if matches!(
                    job.state,
                    JobState::Claimed { .. } | JobState::Running { .. }
                ) {
                    store.request_manager_cancel(
                        &self.identity,
                        &network,
                        &job.conversation_id,
                        &job.trigger,
                    )?;
                    self.publish_changed(
                        &network,
                        &job.conversation_id,
                        Changed::Job(&job.trigger),
                    );
                }
            }
        }
        let local = self.identity.device_id()?;
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
                        &self.identity,
                        &network,
                        &record.conversation,
                        &trigger,
                    )?;
                    self.publish_changed(&network, &record.conversation, Changed::Job(&trigger));
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
            self.publish_changed(
                &network,
                &record.conversation,
                Changed::Work(&record.work_id),
            );
        }
        drop(store);
        if management_revoked {
            let mut tokens = self.agent_tokens.lock().await;
            for invocation in active.values() {
                tokens.remove(&invocation.token);
                let _ = invocation.cancel.send(true);
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
    use crate::{
        config::ManagerSettings,
        crypto::LocalIdentity,
        model::ResolvedModel,
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
        })?;
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
            settings.manager = Some(ManagerSettings {
                provider: Provider::Codex,
                workspace: directory,
                model: "fixture".into(),
                reasoning_effort: None,
                backup: Default::default(),
            });
        }
        {
            let mut providers = node.providers.lock().unwrap();
            providers.switches.execution.insert(Provider::Pi);
            providers
                .bins
                .insert(Provider::Pi, PathBuf::from("/fixture/pi"));
        }
        bot(node, admin, "c", "Ops", &[]).await?;
        let network = node.store.lock().await.network()?.context("network")?;
        let (trigger, attempt) = start_reply(node, admin, "m1").await?;
        let cancel = node
            .register_manager_attempt_fixture(
                "permission-token".into(),
                Access {
                    network_id: network,
                    human: false,
                    conversation_id: Some("c".into()),
                    manager: None,
                },
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
                    "group":0,"after":Value::Null}))?,
            )?;
        }
        let spec = WorkSpec {
            work_id: work_id.into(),
            conversation: "c".into(),
            trigger: "message:m1".into(),
            target: local.clone(),
            dispatcher: local,
            call_ref: work_id.into(),
            kind: WorkKind::Agent(provider),
            model: None,
            resolved: ResolvedModel {
                provider,
                model: None,
            },
            cwd: cwd.to_string_lossy().into_owned(),
            title: work_id.into(),
            prompt: "work".into(),
            continues: None,
            parent: None,
            thread: Some(target.thread_id.clone()),
        };
        let mut store = node.store.lock().await;
        store.accept_work_fixture(&node.identity, network, spec.clone(), now_ms())?;
        let actual_cwd = cwd.canonicalize()?;
        let actual_cwd = actual_cwd.to_str().context("fixture cwd")?;
        store.reserve_work_native_submission(network, &spec, actual_cwd, None)?;
        store.begin_reserved_work_native_input(network, &spec, &target.thread_id, actual_cwd)?;
        store.apply_work_signal(
            &node.identity,
            network,
            work_id,
            Signal::TurnStarted {
                turn_id: target.turn_id.clone(),
                epoch: node.codex.epoch(),
            },
            now_ms(),
        )?;
        Ok(target)
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
            std::os::unix::fs::symlink(&actual, &alias)?;
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
    async fn ordinary_model_and_provider_changes_keep_the_captured_turn() -> Result<()> {
        let (node, admin, directory) = node().await?;
        let (trigger, attempt, access, cancel) = active_turn(&node, &admin, &directory).await?;
        let epoch = node.local_permission_epoch.load(Ordering::Acquire);
        let save = node.config_save.lock().await;
        node.write_providers(|target| {
            target.manager.as_mut().context("manager")?.model = "next-model".into();
            Ok(())
        })
        .await?;
        drop(node.manager_input_lease(&attempt, Provider::Codex).await?);
        node.write_providers(|target| {
            target.manager.as_mut().context("manager")?.provider = Provider::Pi;
            Ok(())
        })
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
            node.store
                .lock()
                .await
                .scheduler_job("c", &trigger)?
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
        std::os::unix::fs::symlink(&retained, &retained_alias)?;
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
            let store = node.store.lock().await;
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
        node.write_providers(|target| {
            target.local.switches.execution.remove(&Provider::Codex);
            Ok(())
        })
        .await?;
        assert!(!*cancel.borrow());
        drop(node.manager_input_lease(&attempt, Provider::Codex).await?);
        assert!(node.work_input_lease(network, "c", &target).await.is_err());
        drop(node.work_input_lease(network, "c", &other).await?);
        node.write_providers(|target| {
            *target.manager = None;
            Ok(())
        })
        .await?;
        drop(save);
        assert!(*cancel.borrow());
        assert!(node.manager_tool_lease(&access).await.is_err());
        let store = node.store.lock().await;
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
        drop(store);
        drop(node);
        std::fs::remove_dir_all(directory)?;
        Ok(())
    }
}
