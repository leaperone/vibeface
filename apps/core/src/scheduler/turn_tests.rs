use super::*;
use crate::{
    crypto::LocalIdentity,
    model::{ResolvedModel, WorkKind},
    providers::Provider,
    runtime::test_support::{access, bot, manager_settings, node, signed_main_input},
    store::bot_settings::ChangeRequest,
    work::WorkSpec,
};
use anyhow::Context as _;
use std::time::Duration;

struct Fixture {
    node: Arc<Runtime>,
    directory: PathBuf,
    network: String,
    scope: SessionScope,
    trigger: String,
}

impl Fixture {
    async fn new(task: bool, mode: &str) -> Result<Self> {
        let (node, admin, directory) = node().await?;
        {
            let mut settings = node.settings.lock().unwrap();
            settings.roots = vec![directory.clone()];
            settings.manager = Some(manager_settings(
                Provider::Pi,
                directory.clone(),
                "fixture".into(),
            ));
            let mut providers = node.providers.lock().unwrap();
            providers.switches.execution = [Provider::Pi].into();
            providers.bins.insert(
                Provider::Pi,
                PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                    .join("tests/fixtures")
                    .join(if cfg!(windows) {
                        "fake-pi.cmd"
                    } else {
                        "fake-pi"
                    }),
            );
            providers.env.apply(
                Provider::Pi,
                BTreeMap::from([
                    (
                        "PI_CODING_AGENT_SESSION_DIR".into(),
                        Some(directory.join("sessions").display().to_string()),
                    ),
                    (
                        "FAKE_PI_RECORDS_DIR".into(),
                        Some(directory.join("pi-log-records").display().to_string()),
                    ),
                    ("FAKE_PI_MODE".into(), Some(mode.into())),
                ]),
            )?;
        }
        node.tool_url.set("http://127.0.0.1:1".into()).unwrap();
        bot(&node, &admin, "c", "Turn lifecycle", &[]).await?;
        let network = node.network_id().await?;
        let (scope, trigger) = Self::admit(&node, &admin, &network, &directory, task).await?;
        Ok(Self {
            node,
            directory,
            network,
            scope,
            trigger,
        })
    }

    async fn admit(
        node: &Runtime,
        admin: &LocalIdentity,
        network: &str,
        directory: &std::path::Path,
        task: bool,
    ) -> Result<(SessionScope, String)> {
        let (identity, admin, network, directory) = (
            node.identity.clone(),
            admin.clone(),
            network.to_owned(),
            directory.to_owned(),
        );
        node.store
            .call("turn_test.admit", move |store| {
                let now = crate::runtime::now_ms();
                store.append_event(
                    &identity,
                    &network,
                    "c",
                    signed_main_input(&admin, &network, "c", "origin", "fixture", vec![], 1)?,
                    vec![],
                    now,
                )?;
                if task {
                    let local = identity.device_id()?;
                    let work_id = WorkSpec::id("c", "message:origin", "turn");
                    store.accept_work_fixture(
                        &identity,
                        &network,
                        WorkSpec {
                            work_id: work_id.clone(),
                            conversation: "c".into(),
                            trigger: "message:origin".into(),
                            target: local.clone(),
                            dispatcher: local,
                            call_ref: "turn".into(),
                            kind: WorkKind::Task,
                            model: None,
                            resolved: ResolvedModel {
                                provider: Provider::Pi,
                                model: Some("fixture".into()),
                            },
                            cwd: directory.to_string_lossy().into_owned(),
                            title: "Turn".into(),
                            prompt: "fixture".into(),
                            continues: None,
                            parent: None,
                            thread: None,
                        },
                        now,
                    )?;
                    Ok((
                        SessionScope::Task {
                            work_id: work_id.clone(),
                        },
                        crate::scheduling::task_trigger(&work_id),
                    ))
                } else {
                    store.change_bot(
                        &identity,
                        &network,
                        "c",
                        "message:origin",
                        "schedule",
                        ChangeRequest::SaveSchedule {
                            schedule_id: None,
                            name: "Turn".into(),
                            prompt: "fixture".into(),
                            first_run_at_ms: now,
                            every_minutes: None,
                            enabled: None,
                        },
                        Provider::Pi,
                        now,
                    )?;
                    let due = store.admit_due_schedules(&identity, &network, now)?;
                    let (_, trigger) = due.into_iter().next().context("due schedule")?;
                    Ok((
                        SessionScope::Routine {
                            trigger: trigger.clone(),
                        },
                        trigger,
                    ))
                }
            })
            .await
    }

    async fn sql(&self, sql: &'static str) -> Result<()> {
        self.node
            .store
            .call("turn_test.sql", move |store| {
                Ok(store.connection.execute_batch(sql)?)
            })
            .await
    }

    async fn run(&self) -> Result<Option<Value>> {
        tokio::time::timeout(
            Duration::from_secs(10),
            super::super::run(
                self.node.clone(),
                access(self.network.clone(), false, Some("c".into())),
                "c".into(),
                self.scope.clone(),
            ),
        )
        .await?
    }

    async fn owner(&self) -> Result<TurnAttempt> {
        let mut owner = TurnAttempt::claim(
            self.node.clone(),
            access(self.network.clone(), false, Some("c".into())),
            "c".into(),
            self.scope.clone(),
        )
        .await?
        .context("claimed turn")?;
        owner.record_initial_runtime().await?;
        let cancel = self
            .node
            .register_manager_attempt(
                owner.phase.round()?.token.clone(),
                owner.access.clone(),
                &owner.attempt.id,
                &owner.job.trigger,
                Provider::Pi,
                owner.environment.permission_epoch,
            )
            .await?;
        owner.phase.register(cancel)?;
        owner.reserve_and_activate().await?;
        Ok(owner)
    }

    async fn facts(&self) -> Result<(SchedulerJob, u64, u64, usize)> {
        let trigger = self.trigger.clone();
        self.node.store.call("turn_test.facts", move |store| {
            let job = store.scheduler_job("c", &trigger)?.context("job")?;
            let budget = store.connection.query_row("SELECT COALESCE(sum(used),0) FROM budget_days", [], |row| row.get(0))?;
            let native = store.connection.query_row(
                "SELECT (SELECT count(*) FROM manager_turns)+(SELECT count(*) FROM manager_processes)", [], |row| row.get(0))?;
            Ok((job, budget, native, store.pending_checks()?.len()))
        }).await
    }

    fn prompts(&self) -> Result<usize> {
        let records = self.directory.join("pi-log-records");
        if !records.exists() {
            return Ok(0);
        }
        let mut count = 0;
        for entry in std::fs::read_dir(records)? {
            let path = entry?.path();
            if path
                .extension()
                .is_some_and(|extension| extension == "json")
            {
                let record: Value = serde_json::from_slice(&std::fs::read(path)?)?;
                count += usize::from(record.get("prompt").is_some());
            }
        }
        Ok(count)
    }

    async fn close(self, result: Result<()>) -> Result<()> {
        self.node.shutdown().await;
        drop(self.node);
        std::fs::remove_dir_all(self.directory)?;
        result
    }
}

#[tokio::test]
async fn admission_errors_release_registration_and_refund_without_native_execution() -> Result<()> {
    for case in ["budget", "activation", "unknown_write"] {
        let fixture = Fixture::new(case != "activation", "reply").await?;
        let result = async {
            fixture.sql("CREATE TABLE activated(count INTEGER); CREATE TABLE charged(count INTEGER);
                CREATE TRIGGER activation_record AFTER UPDATE OF state ON scheduler_jobs WHEN NEW.state='running'
                BEGIN INSERT INTO activated VALUES(1); END;
                CREATE TRIGGER charge_record AFTER INSERT ON budget_days BEGIN INSERT INTO charged VALUES(1); END;").await?;
            fixture.sql(match case {
                "budget" => "CREATE TRIGGER fail_budget BEFORE INSERT ON budget_days
                    BEGIN SELECT RAISE(FAIL,'controlled budget reservation failure'); END;",
                "activation" => "CREATE TRIGGER fail_activation BEFORE UPDATE OF state ON scheduler_jobs WHEN NEW.state='running'
                    BEGIN SELECT RAISE(FAIL,'controlled local activation failure'); END;",
                _ => "CREATE TRIGGER fail_unknown BEFORE UPDATE OF runtime ON scheduler_jobs
                    WHEN json_extract(NEW.runtime,'$.execution')='unknown'
                    BEGIN SELECT RAISE(FAIL,'controlled pre-submission write failure'); END;",
            }).await?;
            assert_eq!(fixture.run().await?.context("failed invocation")?["state"], "failed", "{case}");
            let (job, budget, native, pending) = fixture.facts().await?;
            assert!(matches!(job.state, JobState::Failed { .. }), "{case}: {:?}", job.state);
            assert_eq!((budget, native, pending, fixture.prompts()?), (0, 0, 0, 0), "{case}");
            assert_eq!(job.runtime.context("runtime")?.execution, TurnExecution::NotStarted, "{case}");
            fixture.node.store.call("turn_test.admission_boundaries", move |store| {
                let activated: u64 = store.connection.query_row("SELECT count(*) FROM activated", [], |row| row.get(0))?;
                let charged: u64 = store.connection.query_row("SELECT count(*) FROM charged", [], |row| row.get(0))?;
                // AgentTask registration marks Running before budget admission and local publication.
                assert_eq!((activated, charged), match case { "budget" => (1, 0), "activation" => (0, 1), _ => (1, 1) });
                Ok(())
            }).await?;
            assert!(fixture.node.agent_tokens.lock().await.is_empty());
            assert!(fixture.node.manager_controls.lock().await.is_empty());
            Ok(())
        }.await;
        fixture.close(result).await?;
    }
    Ok(())
}

#[tokio::test]
async fn a_failed_primary_refund_consumes_its_reservation_before_the_final_window() -> Result<()> {
    let fixture = Fixture::new(false, "reply").await?;
    let result = async {
        let mut owner = fixture.owner().await?;
        fixture.sql("CREATE TRIGGER fail_refund BEFORE UPDATE OF used ON budget_days WHEN NEW.used < OLD.used
            BEGIN SELECT RAISE(FAIL,'controlled refund failure'); END;").await?;
        assert!(owner.refund_budget().await.is_err());
        assert!(owner.budget.is_none());
        fixture.sql("DROP TRIGGER fail_refund;").await?;
        assert_eq!(owner.finish(Err(anyhow::anyhow!("controlled local failure"))).await?["state"], "failed");
        let (_, budget, native, pending) = fixture.facts().await?;
        // If finish reuses the failed refund's reservation, dropping the fault would make this 0.
        assert_eq!((budget, native, pending, fixture.prompts()?), (1, 0, 0, 0));
        assert!(fixture.node.agent_tokens.lock().await.is_empty());
        assert!(fixture.node.manager_controls.lock().await.is_empty());
        Ok(())
    }.await;
    fixture.close(result).await
}

#[tokio::test]
async fn cancellation_requires_a_completed_fence_and_unknown_keeps_its_charge() -> Result<()> {
    for failed_fence in [false, true] {
        let fixture = Fixture::new(false, "reply").await?;
        let result = async {
            let owner = fixture.owner().await?;
            if failed_fence {
                fixture.sql("CREATE TRIGGER fail_fence BEFORE UPDATE OF permission_version ON scheduler_sessions
                    WHEN NEW.permission_version>OLD.permission_version AND NOT EXISTS(SELECT 1 FROM pending_checks)
                    BEGIN SELECT RAISE(FAIL,'controlled persistent fence failure'); END;").await?;
            }
            let invocation = owner.finish(Err(ManagerError {
                stage: ProviderStage::Validate, outcome: manager::ManagerFailure::Cancelled,
                start: manager::StartEvidence::NotStarted, session_id: None,
                failure: ExecFailure::new(FailureKind::Unknown, "controlled pre-submission cancellation"),
            }.into())).await?;
            assert_eq!(invocation["state"], if failed_fence { "reconciling" } else { "cancelled" });
            let (job, budget, native, pending) = fixture.facts().await?;
            assert_eq!((budget, native, pending, fixture.prompts()?), if failed_fence { (1, 0, 1, 0) } else { (0, 0, 0, 0) });
            assert!(matches!((failed_fence, job.state), (true, JobState::Reconciling { .. }) | (false, JobState::Cancelled { .. })));
            assert!(fixture.node.agent_tokens.lock().await.is_empty());
            assert!(fixture.node.manager_controls.lock().await.is_empty());
            Ok(())
        }.await;
        fixture.close(result).await?;
    }
    Ok(())
}

#[tokio::test]
async fn a_successful_sdk_return_with_a_record_failure_is_never_submitted_again() -> Result<()> {
    let fixture = Fixture::new(false, "reply").await?;
    let result = async {
        fixture.sql("CREATE TRIGGER fail_success_record BEFORE UPDATE OF runtime ON scheduler_jobs
            WHEN json_extract(NEW.runtime,'$.stage')='record' AND json_extract(NEW.runtime,'$.execution')='started'
            BEGIN SELECT RAISE(FAIL,'controlled successful return record failure'); END;").await?;
        assert_eq!(fixture.run().await?.context("invocation")?["state"], "reconciling");
        let (job, budget, _, pending) = fixture.facts().await?;
        assert!(matches!(job.state, JobState::Reconciling { .. }));
        assert_eq!((budget, pending, fixture.prompts()?), (1, 1, 1));
        assert_eq!(job.runtime.context("runtime")?.execution, TurnExecution::Unknown);
        let network = fixture.network.clone();
        fixture.node.store.call("turn_test.no_premature_reply", move |store| {
            assert_eq!(store.messages(&network, "c")?.into_iter().filter(|message| message.role == Role::Assistant).count(), 0);
            Ok(())
        }).await?;
        assert!(fixture.node.agent_tokens.lock().await.is_empty());
        assert!(fixture.node.manager_controls.lock().await.is_empty());
        Ok(())
    }.await;
    fixture.close(result).await
}

#[tokio::test]
async fn a_registered_backup_keeps_one_reservation_and_closes_before_a_failed_switch() -> Result<()>
{
    for failed_switch in [false, true] {
        let fixture = Fixture::new(false, "refuse-model").await?;
        let result = async {
            fixture.node.settings.lock().unwrap().manager.as_mut().context("manager")?.backup = BackupPolicy {
                automatic: true,
                candidates: vec![BackupModel { provider: Provider::Pi, model: "backup".into(), reasoning_effort: None }],
            };
            fixture.sql("CREATE TABLE charges(count INTEGER);
                CREATE TRIGGER record_charge AFTER INSERT ON budget_days BEGIN INSERT INTO charges VALUES(1); END;").await?;
            if failed_switch {
                fixture.sql("CREATE TRIGGER fail_switch BEFORE UPDATE OF session_generation ON scheduler_sessions
                    WHEN NEW.session_generation > OLD.session_generation
                    BEGIN SELECT RAISE(FAIL,'controlled backup switch write failure'); END;").await?;
            }
            assert_eq!(fixture.run().await?.context("invocation")?["state"], "failed");
            let (job, budget, native, pending) = fixture.facts().await?;
            assert_eq!((budget, native, pending, fixture.prompts()?), (1, 0, 0, if failed_switch { 1 } else { 2 }));
            let runtime = job.runtime.context("runtime")?;
            assert_eq!(runtime.execution, TurnExecution::NotStarted);
            assert_eq!(runtime.model.as_deref(), Some(if failed_switch { "fixture" } else { "backup" }));
            assert_eq!(runtime.switch_reason, if failed_switch { None } else { Some(FailureKind::NoModel) });
            fixture.node.store.call("turn_test.single_charge", |store| {
                let charges: u64 = store.connection.query_row("SELECT count(*) FROM charges", [], |row| row.get(0))?;
                assert_eq!(charges, 1);
                Ok(())
            }).await?;
            assert!(fixture.node.agent_tokens.lock().await.is_empty());
            assert!(fixture.node.manager_controls.lock().await.is_empty());
            Ok(())
        }.await;
        fixture.close(result).await?;
    }
    Ok(())
}

#[tokio::test]
async fn a_readiness_backup_commits_its_result_with_the_original_admission() -> Result<()> {
    let fixture = Fixture::new(false, "reply").await?;
    let result = async {
        {
            let mut settings = fixture.node.settings.lock().unwrap();
            let manager = settings.manager.as_mut().context("manager")?;
            manager.provider = Provider::Claude;
            manager.backup = BackupPolicy {
                automatic: true,
                candidates: vec![BackupModel {
                    provider: Provider::Pi,
                    model: "backup".into(),
                    reasoning_effort: None,
                }],
            };
            let mut providers = fixture.node.providers.lock().unwrap();
            providers.switches.execution = [Provider::Claude, Provider::Pi].into();
            providers
                .bins
                .insert(Provider::Claude, fixture.directory.join("missing-claude"));
        }
        assert_eq!(
            fixture.run().await?.context("invocation")?["state"],
            "completed"
        );
        let (job, budget, native, pending) = fixture.facts().await?;
        assert_eq!((budget, native, pending, fixture.prompts()?), (1, 0, 0, 1));
        let runtime = job.runtime.context("runtime")?;
        assert_eq!(runtime.provider, Provider::Pi);
        assert_eq!(runtime.model.as_deref(), Some("backup"));
        assert_eq!(
            runtime
                .switched_from
                .context("previous selection")?
                .provider,
            Provider::Claude
        );
        let trigger = fixture.trigger.clone();
        let network = fixture.network.clone();
        fixture
            .node
            .store
            .call("turn_test.one_reply", move |store| {
                assert_eq!(
                    store
                        .scheduler_jobs("c")?
                        .into_iter()
                        .filter(|job| job.trigger == trigger)
                        .count(),
                    1
                );
                let replies: Vec<_> = store
                    .messages(&network, "c")?
                    .into_iter()
                    .filter(|message| message.role == Role::Assistant)
                    .collect();
                assert_eq!(replies.len(), 1);
                assert_eq!(replies[0].text, "Hello");
                Ok(())
            })
            .await?;
        assert!(fixture.node.agent_tokens.lock().await.is_empty());
        assert!(fixture.node.manager_controls.lock().await.is_empty());
        Ok(())
    }
    .await;
    fixture.close(result).await
}
