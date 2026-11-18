use super::super::bot_settings::ChangeRequest;
use super::super::test_support::{
    Net, bot, bot_with_message, conversation, isolated_task, memory_test_shown, net, save_schedule,
    say, second_bot, store,
};
use super::super::*;
use crate::providers::Provider;
use crate::scheduling::{JobOutcome, JobState, SchedulerJob, Shown, schedule_trigger};

fn isolated_claim(
    store: &mut Store,
    net: &Net,
    scope: crate::scheduling::SessionScope,
    provider: Provider,
    limits: crate::config::SchedulingLimits,
) -> Result<Option<crate::scheduling::ClaimedJob>> {
    store.claim_next_in_session(
        &net.a,
        &net.network,
        &crate::scheduling::SessionKey {
            conversation_id: "c".into(),
            scope,
        },
        "boot",
        &crate::scheduling::SessionConfig {
            stable_fingerprint: "stable".into(),
            runtime_fingerprint: "runtime".into(),
            provider,
        },
        &limits,
    )
}

#[test]
fn parallel_sessions_keep_native_tokens_shown_and_memory_writers_separate() -> Result<()> {
    use crate::scheduling::{SessionScope, task_trigger};
    let net = net()?;
    let (mut store, _) = bot_with_message(&net)?;
    let limits = crate::config::SchedulingLimits::default();
    let task1 = isolated_task(&mut store, &net, "parallel-1")?;
    isolated_task(&mut store, &net, "parallel-2")?;
    say(&mut store, &net, "parallel-human", true)?;
    assert!(
        isolated_claim(
            &mut store,
            &net,
            SessionScope::Task {
                work_id: task1.work_id.clone()
            },
            Provider::Codex,
            limits
        )?
        .is_none(),
        "human admission wins even over an older task"
    );
    let main = isolated_claim(
        &mut store,
        &net,
        SessionScope::Main,
        Provider::Codex,
        limits,
    )?
    .context("main")?;
    let first = isolated_claim(
        &mut store,
        &net,
        SessionScope::Task {
            work_id: task1.work_id.clone(),
        },
        Provider::Codex,
        limits,
    )?
    .context("first task")?;
    let second = isolated_claim(
        &mut store,
        &net,
        SessionScope::Task {
            work_id: "parallel-2".into(),
        },
        Provider::Codex,
        limits,
    )?
    .context("second task")?;
    assert_eq!(store.scheduler_jobs("c")?.len(), 3);
    assert!(!store.accept_work_fixture(&net.a, &net.network, task1, 5)?.1);
    assert_eq!(
        store.scheduler_jobs("c")?.len(),
        3,
        "retry keeps the same task job"
    );
    for (index, claimed) in [&main, &first, &second].into_iter().enumerate() {
        let attempt = claimed.job.state.attempt().context("attempt")?;
        let native = format!("native-{index}");
        store.mark_started("c", &claimed.job.trigger, &attempt.id)?;
        store.save_scheduler_native_id(
            "c",
            &claimed.job.trigger,
            &attempt.id,
            &native,
            &format!("token-{index}"),
            false,
        )?;
        let shown = Shown {
            bot: Some(format!("bot-{index}")),
            memory: Some([(format!("memory-{index}"), format!("digest-{index}"))].into()),
            ..Shown::default()
        };
        store.record_manager_turn(&attempt.id, "fixture", &native, Some("turn"), &shown)?;
    }
    let main_attempt = main.job.state.attempt().context("main attempt")?;
    store.stage_memory(
        &net.network,
        "c",
        &main.job.trigger,
        &main_attempt.id,
        "parallel-memory",
        ChangeRequest::Remember {
            memory_id: None,
            text: "prefers isolated tasks".into(),
        },
        6,
    )?;
    assert_eq!(store.scheduler_memory_staged_count("c")?, 1);
    let first_attempt = first.job.state.attempt().context("first attempt")?;
    store.finish_job(
        &net.a,
        &net.network,
        "c",
        &first.job.trigger,
        &first_attempt.id,
        JobOutcome::Completed,
        "task done",
        7,
    )?;
    store.finish_job(
        &net.a,
        &net.network,
        "c",
        &first.job.trigger,
        &first_attempt.id,
        JobOutcome::Completed,
        "task done",
        8,
    )?;
    assert_eq!(
        store
            .staged_memory_overlay("c", &main.job.trigger, &main_attempt.id)?
            .len(),
        1,
        "task completion leaves the main writer alone"
    );
    assert!(store.memories(&net.network, "c")?.is_empty());
    for (index, claimed) in [&main, &first, &second].into_iter().enumerate() {
        let (native, token, shown): (String, String, String) = store.connection.query_row(
            "SELECT native_id,native_token,shown FROM scheduler_sessions WHERE conversation_id='c' AND scope=?",
            [claimed.job.session_scope().storage_key()], |row| Ok((row.get(0)?,row.get(1)?,row.get(2)?)))?;
        assert_eq!(native, format!("native-{index}"));
        assert_eq!(token, format!("token-{index}"));
        let shown: Shown = serde_json::from_str(&shown)?;
        assert_eq!(shown.bot, Some(format!("bot-{index}")));
        assert_eq!(
            shown.memory,
            Some([(format!("memory-{index}"), format!("digest-{index}"))].into())
        );
    }
    store.request_manager_cancel(&net.a, &net.network, "c", &main.job.trigger)?;
    assert!(!store.manager_attempt_allowed_with_version(
        &net.network,
        &net.a.device_id()?,
        "c",
        &main.job.trigger,
        &main_attempt.id,
        main_attempt.permission_version
    )?);
    assert!(
        store.manager_attempt_allowed_with_version(
            &net.network,
            &net.a.device_id()?,
            "c",
            &second.job.trigger,
            &second.job.state.attempt().context("second attempt")?.id,
            second
                .job
                .state
                .attempt()
                .context("second attempt")?
                .permission_version
        )?
    );
    assert!(matches!(
        store
            .scheduler_job("c", &task_trigger("parallel-2"))?
            .context("second")?
            .state,
        JobState::Running { .. }
    ));
    let finished = context_after(&store.connection, &net.network, "c", None, &VersionVector::new())?.events.into_iter().filter(|event| {
        matches!(&event.event.body, EventBody::WorkFinished { work_id, .. } if work_id == "parallel-1")
    }).count();
    assert_eq!(finished, 1);
    Ok(())
}

#[test]
fn serial_provider_gates_only_its_actual_bot_instance() -> Result<()> {
    use crate::scheduling::{QueueHoldReason, SessionConfig, SessionKey, SessionScope};
    let net = net()?;
    let (mut store, _) = bot_with_message(&net)?;
    second_bot(&mut store, &net, "other")?;
    let limits = crate::config::SchedulingLimits::default();
    say(&mut store, &net, "serial-human", true)?;
    isolated_claim(
        &mut store,
        &net,
        SessionScope::Main,
        Provider::Cursor,
        limits,
    )?
    .context("main")?;
    isolated_task(&mut store, &net, "serial-task")?;
    let task = store
        .scheduler_job("c", "task:serial-task")?
        .context("task")?;
    assert_eq!(
        store.scheduler_job_hold(
            &net.network,
            &net.a.device_id()?,
            &task,
            Provider::Cursor,
            &limits
        )?,
        Some(QueueHoldReason::ProviderSerial)
    );
    assert!(
        isolated_claim(
            &mut store,
            &net,
            task.session_scope(),
            Provider::Cursor,
            limits
        )?
        .is_none()
    );
    store.message_bot(
        &net.a,
        &net.network,
        "c",
        "message:m1",
        "serial-other",
        "d",
        "other input",
        5,
    )?;
    let other = store
        .claim_next_in_session(
            &net.a,
            &net.network,
            &SessionKey {
                conversation_id: "d".into(),
                scope: SessionScope::Main,
            },
            "boot",
            &SessionConfig {
                stable_fingerprint: "stable".into(),
                runtime_fingerprint: "runtime".into(),
                provider: Provider::Cursor,
            },
            &limits,
        )?
        .context("other bot is not this Cursor instance")?;
    assert_eq!(other.job.conversation_id, "d");
    Ok(())
}

#[test]
fn held_tasks_leave_room_for_humans_and_machine_slots_are_real() -> Result<()> {
    use crate::scheduling::{QueueHoldReason, SessionScope};
    let net = net()?;
    let (mut store, _) = bot_with_message(&net)?;
    let limits = crate::config::SchedulingLimits {
        machine_slots: 2,
        task_limit: 1,
    };
    isolated_task(&mut store, &net, "limited-1")?;
    isolated_task(&mut store, &net, "limited-2")?;
    isolated_claim(
        &mut store,
        &net,
        SessionScope::Task {
            work_id: "limited-1".into(),
        },
        Provider::Codex,
        limits,
    )?
    .context("first")?;
    let task = store
        .scheduler_job("c", "task:limited-2")?
        .context("second")?;
    assert_eq!(
        store.scheduler_job_hold(
            &net.network,
            &net.a.device_id()?,
            &task,
            Provider::Codex,
            &limits
        )?,
        Some(QueueHoldReason::TaskLimit)
    );
    say(&mut store, &net, "priority-human", true)?;
    isolated_claim(
        &mut store,
        &net,
        SessionScope::Main,
        Provider::Codex,
        limits,
    )?
    .context("human bypasses held task")?;
    let more_tasks = crate::config::SchedulingLimits {
        machine_slots: 2,
        task_limit: 2,
    };
    assert_eq!(
        store.scheduler_job_hold(
            &net.network,
            &net.a.device_id()?,
            &task,
            Provider::Codex,
            &more_tasks
        )?,
        Some(QueueHoldReason::MachineSlot)
    );
    assert!(
        isolated_claim(
            &mut store,
            &net,
            task.session_scope(),
            Provider::Codex,
            more_tasks
        )?
        .is_none()
    );
    Ok(())
}

fn record_execution_evidence_fixture(
    store: &mut Store,
    net: &Net,
    job: &SchedulerJob,
    evidence: &str,
) -> Result<()> {
    use crate::scheduling::TurnExecution;
    use vibeface_bridge::failure::ProviderStage;

    let attempt = job.state.attempt().context("attempt")?;
    match evidence {
        "unknown" | "started" => {
            store.update_turn_runtime(
                "c",
                &job.trigger,
                &attempt.id,
                ProviderStage::TurnStart,
                if evidence == "unknown" {
                    TurnExecution::Unknown
                } else {
                    TurnExecution::Started
                },
            )?;
        }
        "tool" => store.record_manager_tool_call("c", &job.trigger, &attempt.id)?,
        "process" => store.record_manager_process(&attempt.id, 123)?,
        "native" => store.record_manager_locator(&attempt.id, "system", "thread", None)?,
        "pending" => {
            store.connection.execute(
            "INSERT INTO pending_checks(attempt_id,conversation_id,trigger,detail,created_at_ms) VALUES(?,'c',?,'unknown',0)",
            params![attempt.id, job.trigger])?;
        }
        "running" => {
            store.mark_started("c", &job.trigger, &attempt.id)?;
        }
        "cancelling" => {
            store.request_manager_cancel(&net.a, &net.network, "c", &job.trigger)?;
        }
        _ => {}
    }
    Ok(())
}

#[test]
fn abandoned_claims_requeue_only_without_execution_evidence_and_fence_the_old_attempt() -> Result<()>
{
    use crate::scheduling::{SessionScope, TurnExecution, TurnRuntime};
    use vibeface_bridge::failure::ProviderStage;

    for evidence in [
        "missing_runtime",
        "not_started",
        "unknown",
        "started",
        "tool",
        "process",
        "native",
        "pending",
        "running",
        "cancelling",
    ] {
        for restarted in [false, true] {
            let net = net()?;
            let (mut store, _) = bot_with_message(&net)?;
            say(&mut store, &net, "abandoned", true)?;
            let claimed = isolated_claim(
                &mut store,
                &net,
                SessionScope::Main,
                Provider::Codex,
                crate::config::SchedulingLimits::default(),
            )?
            .context("claim")?;
            let job = claimed.job;
            let attempt = job.state.attempt().context("attempt")?.clone();
            if evidence != "missing_runtime" {
                store.record_turn_runtime(
                    "c",
                    &job.trigger,
                    &attempt.id,
                    &TurnRuntime {
                        provider: Provider::Codex,
                        model: None,
                        reasoning_effort: None,
                        generation: "fixture".into(),
                        session_generation: claimed.session_generation,
                        delivery_id: attempt.id.clone(),
                        stage: ProviderStage::Validate,
                        execution: TurnExecution::NotStarted,
                        attempted_at_ms: 0,
                        switched_from: None,
                        switch_reason: None,
                    },
                )?;
            }
            record_execution_evidence_fixture(&mut store, &net, &job, evidence)?;
            let unstarted = matches!(evidence, "missing_runtime" | "not_started");
            if restarted {
                store.recover_scheduler_jobs(&net.a, &net.network, "next-boot")?;
            } else {
                assert_eq!(
                    store.requeue_abandoned_scheduler_claim(
                        "c",
                        &job.trigger,
                        &attempt.id,
                        "boot"
                    )?,
                    unstarted,
                    "{evidence}"
                );
            }
            let current = store
                .scheduler_job("c", &job.trigger)?
                .context("retained job")?;
            if unstarted {
                assert_eq!(current.state, JobState::Queued, "{evidence}");
                assert!(current.runtime.is_none());
                assert!(store.pending_checks()?.is_empty());
                assert!(!store.manager_attempt_allowed_with_version(
                    &net.network,
                    &net.a.device_id()?,
                    "c",
                    &job.trigger,
                    &attempt.id,
                    attempt.permission_version
                )?);
                assert!(!store.requeue_abandoned_scheduler_claim(
                    "c",
                    &job.trigger,
                    &attempt.id,
                    "boot"
                )?);
                store.request_manager_cancel(&net.a, &net.network, "c", &job.trigger)?;
                store.scheduler_maintenance_idle()?;
            } else if restarted {
                assert!(
                    matches!(current.state, JobState::Reconciling { .. }),
                    "{evidence}"
                );
                assert_eq!(store.pending_checks()?.len(), 1, "{evidence}");
            } else {
                assert_ne!(current.state, JobState::Queued, "{evidence}");
            }
        }
    }
    Ok(())
}

#[test]
fn scheduler_refunds_only_live_unstarted_reservations_on_their_original_utc_day() -> Result<()> {
    for evidence in [
        "not_started",
        "unknown",
        "started",
        "tool",
        "process",
        "native",
        "pending",
        "restarted",
    ] {
        let net = net()?;
        let (mut store, _) = bot_with_message(&net)?;
        say(&mut store, &net, "budget", true)?;
        let job = store
            .claim_next_fixture(&net.a, &net.network, "c", "boot", "budget")?
            .context("claim")?
            .job;
        let reservation = store.reserve_scheduler_budget(&net.network, &job, 3, 5, 86_399_999)?;
        assert!(
            store
                .reserve_scheduler_budget(&net.network, &job, 3, 5, 86_399_999)
                .is_err()
        );
        store.connection.execute(
            "INSERT INTO budget_days(network_id,day,used) VALUES(?,'1970-01-02',4)",
            [&net.network],
        )?;
        record_execution_evidence_fixture(&mut store, &net, &job, evidence)?;
        let refunded = evidence == "not_started";
        assert_eq!(
            store.refund_scheduler_budget(
                reservation,
                if evidence == "restarted" {
                    "next-boot"
                } else {
                    "boot"
                }
            )?,
            refunded,
            "{evidence}"
        );
        let used = |day: &str| -> Result<u64> {
            Ok(store.connection.query_row(
                "SELECT used FROM budget_days WHERE network_id=? AND day=?",
                params![net.network, day],
                |row| row.get(0),
            )?)
        };
        assert_eq!(
            used("1970-01-01")?,
            if refunded { 0 } else { 3 },
            "{evidence}"
        );
        assert_eq!(
            used("1970-01-02")?,
            4,
            "refund cannot debit the next UTC day"
        );
    }
    Ok(())
}

#[test]
fn scheduler_budget_reservations_share_an_atomic_daily_limit() -> Result<()> {
    let net = net()?;
    let directory = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../.artifacts")
        .join(format!("scheduler-budget-{}", uuid::Uuid::new_v4()));
    crate::config::private_dir(&directory)?;
    let path = directory.join("node.sqlite3");
    let mut original = Store::open(&path)?;
    original.install_trust_bundle(&net.network, &net.state)?;
    conversation(&mut original, &net)?;
    original.append_event(&net.a, &net.network, "c", bot(&net, "Helper")?, vec![], 2)?;
    say(&mut original, &net, "concurrent-budget", true)?;
    let job = original
        .claim_next_fixture(&net.a, &net.network, "c", "boot", "budget")?
        .context("claim")?
        .job;
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(8));
    let mut handles = Vec::new();
    for _ in 0..8 {
        let mut connection = Store::open(&path)?;
        let (network, job, barrier) = (net.network.clone(), job.clone(), barrier.clone());
        handles.push(std::thread::spawn(move || {
            barrier.wait();
            connection
                .reserve_scheduler_budget(&network, &job, 1, 3, 0)
                .is_ok()
        }));
    }
    let accepted = handles
        .into_iter()
        .map(|handle| handle.join().expect("budget thread"))
        .filter(|accepted| *accepted)
        .count();
    assert_eq!(accepted, 3);
    let used: u64 = original.connection.query_row(
        "SELECT used FROM budget_days WHERE network_id=? AND day='1970-01-01'",
        [&net.network],
        |row| row.get(0),
    )?;
    assert_eq!(used, 3);
    drop(original);
    std::fs::remove_dir_all(directory)?;
    Ok(())
}

#[test]
fn restart_recovers_each_scope_and_unknown_turns_are_never_replayed() -> Result<()> {
    use crate::scheduling::{QueueHoldReason, SessionScope};
    let net = net()?;
    let (mut store, _) = bot_with_message(&net)?;
    let limits = crate::config::SchedulingLimits {
        machine_slots: 3,
        task_limit: 2,
    };
    say(&mut store, &net, "restart-human", true)?;
    isolated_task(&mut store, &net, "restart-task")?;
    let main = isolated_claim(
        &mut store,
        &net,
        SessionScope::Main,
        Provider::Codex,
        limits,
    )?
    .context("main")?;
    let task = isolated_claim(
        &mut store,
        &net,
        SessionScope::Task {
            work_id: "restart-task".into(),
        },
        Provider::Codex,
        limits,
    )?
    .context("task")?;
    let main_attempt = main.job.state.attempt().context("main attempt")?;
    store.mark_started("c", &main.job.trigger, &main_attempt.id)?;
    store.recover_scheduler_jobs(&net.a, &net.network, "new-boot")?;
    assert!(matches!(
        store
            .scheduler_job("c", &main.job.trigger)?
            .context("main")?
            .state,
        JobState::Reconciling { .. }
    ));
    assert!(matches!(
        store
            .scheduler_job("c", &task.job.trigger)?
            .context("task")?
            .state,
        JobState::Queued
    ));
    assert_eq!(store.pending_checks()?.len(), 1);
    say(&mut store, &net, "after-restart-human", true)?;
    let next = store
        .scheduler_job("c", "message:after-restart-human")?
        .context("next")?;
    assert_eq!(
        store.scheduler_job_hold(
            &net.network,
            &net.a.device_id()?,
            &next,
            Provider::Codex,
            &limits
        )?,
        Some(QueueHoldReason::PendingCheck)
    );
    let config = crate::scheduling::SessionConfig {
        stable_fingerprint: "stable".into(),
        runtime_fingerprint: "runtime".into(),
        provider: Provider::Codex,
    };
    let recovered = store
        .claim_next_in_session(
            &net.a,
            &net.network,
            &task.job.session_key(),
            "new-boot",
            &config,
            &limits,
        )?
        .context("unstarted task can run in its own scope")?;
    assert_ne!(
        recovered.job.state.attempt().context("new attempt")?.id,
        task.job.state.attempt().context("old attempt")?.id
    );
    assert_eq!(
        store.pending_checks()?.len(),
        1,
        "claiming another scope does not clear unknown evidence"
    );
    let check = store
        .pending_checks()?
        .into_iter()
        .find(|check| check.attempt_id == main_attempt.id)
        .context("pending check")?;
    store.confirm_manager_stopped(&net.a, &net.network, &check, 20)?;
    assert_eq!(
        store.scheduler_job_hold(
            &net.network,
            &net.a.device_id()?,
            &next,
            Provider::Codex,
            &limits
        )?,
        None
    );
    assert!(matches!(
        store
            .scheduler_job("c", &main.job.trigger)?
            .context("old job")?
            .state,
        JobState::Interrupted { .. }
    ));
    Ok(())
}

#[test]
fn a_routine_is_ephemeral_and_keeps_the_main_session_untouched() -> Result<()> {
    use crate::scheduling::{SessionConfig, SessionScope};
    let net = net()?;
    let (mut store, _) = bot_with_message(&net)?;
    let (saved, _) = store.change_bot(
        &net.a,
        &net.network,
        "c",
        "message:m1",
        "routine",
        save_schedule(None, "Digest", 5),
        Provider::Codex,
        3,
    )?;
    let BotChange::Schedule { schedule_id, .. } = saved.change else {
        panic!("schedule")
    };
    let due = store.admit_due_schedules(&net.a, &net.network, 1_001)?;
    assert_eq!(due.len(), 1);
    say(&mut store, &net, "routine-main", true)?;
    let limits = crate::config::SchedulingLimits::default();
    let main = isolated_claim(
        &mut store,
        &net,
        SessionScope::Main,
        Provider::Codex,
        limits,
    )?
    .context("main")?;
    let main_attempt = main.job.state.attempt().context("main attempt")?;
    store.save_scheduler_native_id(
        "c",
        &main.job.trigger,
        &main_attempt.id,
        "main-native",
        "main-token",
        true,
    )?;
    let trigger = schedule_trigger(&schedule_id, 1_000);
    let routine_key = crate::scheduling::SessionKey {
        conversation_id: "c".into(),
        scope: SessionScope::Routine {
            trigger: trigger.clone(),
        },
    };
    let config = SessionConfig {
        stable_fingerprint: "stable".into(),
        runtime_fingerprint: "runtime".into(),
        provider: Provider::Codex,
    };
    let routine = store
        .claim_next_in_session(&net.a, &net.network, &routine_key, "boot", &config, &limits)?
        .context("routine runs independently")?;
    assert!(routine.previous_native_id.is_none());
    assert!(routine.previous_token.is_none());
    assert!(routine.covers.is_none());
    assert!(routine.shown.memory.is_none());
    let attempt = routine.job.state.attempt().context("routine attempt")?;
    store.save_scheduler_native_id(
        "c",
        &trigger,
        &attempt.id,
        "routine-native",
        "routine-token",
        true,
    )?;
    store.finish_job(
        &net.a,
        &net.network,
        "c",
        &trigger,
        &attempt.id,
        JobOutcome::Completed,
        "digest",
        2_000,
    )?;
    let routine_cache: (Option<String>, Option<String>) = store.connection.query_row("SELECT native_id,native_token FROM scheduler_sessions WHERE conversation_id='c' AND scope=?", [routine_key.scope.storage_key()], |row| Ok((row.get(0)?,row.get(1)?)))?;
    assert_eq!(routine_cache, (None, None));
    let main_cache: (String,String) = store.connection.query_row("SELECT native_id,native_token FROM scheduler_sessions WHERE conversation_id='c' AND scope='main'", [], |row| Ok((row.get(0)?,row.get(1)?)))?;
    assert_eq!(main_cache, ("main-native".into(), "main-token".into()));
    assert_eq!(
        store
            .message(&net.network, "c", &routine.job.message_id)?
            .context("ordinary routine event")?
            .text,
        "digest"
    );
    Ok(())
}

#[test]
fn stable_runtime_and_provider_changes_have_distinct_session_lineage() -> Result<()> {
    use crate::scheduling::{SessionConfig, SessionKey, SessionScope};
    let net = net()?;
    let (mut store, _) = bot_with_message(&net)?;
    let limits = crate::config::SchedulingLimits::default();
    let key = SessionKey {
        conversation_id: "c".into(),
        scope: SessionScope::Main,
    };
    let old = SessionConfig {
        stable_fingerprint: "stable-one".into(),
        runtime_fingerprint: "runtime-one".into(),
        provider: Provider::Codex,
    };
    say(&mut store, &net, "config-one", true)?;
    let first = store
        .claim_next_in_session(&net.a, &net.network, &key, "boot", &old, &limits)?
        .context("first")?;
    let attempt = first.job.state.attempt().context("attempt")?;
    store.save_scheduler_native_id(
        "c",
        &first.job.trigger,
        &attempt.id,
        "old-native",
        "old-token",
        true,
    )?;
    let new = SessionConfig {
        stable_fingerprint: "stable-two".into(),
        runtime_fingerprint: "runtime-two".into(),
        provider: Provider::Cursor,
    };
    let status = store.scheduler_session_status(&key, Some(&new))?;
    assert!(status.resumable && status.pending_generation);
    assert!(
        store.manager_attempt_allowed_with_version(
            &net.network,
            &net.a.device_id()?,
            "c",
            &first.job.trigger,
            &attempt.id,
            attempt.permission_version
        )?,
        "configuration observation does not fence an active old turn"
    );
    store.finish_job(
        &net.a,
        &net.network,
        "c",
        &first.job.trigger,
        &attempt.id,
        JobOutcome::Completed,
        "done",
        5,
    )?;
    assert!(
        store
            .scheduler_session_status(&key, Some(&new))?
            .pending_generation
    );
    say(&mut store, &net, "config-two", true)?;
    let second = store
        .claim_next_in_session(&net.a, &net.network, &key, "boot", &new, &limits)?
        .context("second")?;
    let invalidation = second.invalidation.context("invalidation")?;
    assert_eq!(invalidation.reason, "provider_changed");
    assert_eq!(
        invalidation.old_stable_fingerprint.as_deref(),
        Some("stable-one")
    );
    assert_eq!(invalidation.new_stable_fingerprint, "stable-two");
    assert_eq!(
        invalidation.old_runtime_fingerprint.as_deref(),
        Some("runtime-one")
    );
    assert_eq!(invalidation.new_runtime_fingerprint, "runtime-two");
    assert_eq!(invalidation.source_provider, Some(Provider::Codex));
    assert_eq!(invalidation.target_provider, Provider::Cursor);
    assert_eq!(invalidation.source_basis, first.job.basis);
    assert_ne!(invalidation.source_basis, second.job.basis);
    assert_eq!(
        second.shown.session_lineage[0].source_basis,
        first.job.basis
    );
    assert_eq!(second.shown.session_generation, 2);
    assert!(second.previous_native_id.is_none() && second.previous_token.is_none());
    assert_eq!(
        second.shown.session_lineage[0].stable_fingerprint,
        "stable-one"
    );
    assert_eq!(
        second.shown.session_lineage[0].runtime_fingerprint,
        "runtime-one"
    );
    assert_eq!(second.shown.session_lineage[0].provider, Provider::Codex);
    assert!(!store.manager_attempt_allowed_with_version(
        &net.network,
        &net.a.device_id()?,
        "c",
        &first.job.trigger,
        &attempt.id,
        attempt.permission_version
    )?);
    Ok(())
}

fn switchable_scheduler_session(
    net: &Net,
) -> Result<(
    Store,
    crate::scheduling::ClaimedJob,
    crate::scheduling::SessionConfig,
)> {
    use crate::scheduling::{SessionConfig, SessionKey, SessionScope, TurnExecution, TurnRuntime};
    use vibeface_bridge::failure::ProviderStage;
    let mut store = store(net)?;
    conversation(&mut store, net)?;
    store.append_event(&net.a, &net.network, "c", bot(net, "Helper")?, vec![], 2)?;
    say(&mut store, net, "m1", true)?;
    let config = SessionConfig {
        stable_fingerprint: "stable-before-backup".into(),
        runtime_fingerprint: "runtime-before-backup".into(),
        provider: Provider::Codex,
    };
    let key = SessionKey {
        conversation_id: "c".into(),
        scope: SessionScope::Main,
    };
    let limits = crate::config::SchedulingLimits::default();
    let first = store
        .claim_next_in_session(&net.a, &net.network, &key, "boot", &config, &limits)?
        .context("first turn")?;
    let attempt = first
        .job
        .state
        .attempt()
        .context("first attempt")?
        .id
        .clone();
    store.mark_started("c", &first.job.trigger, &attempt)?;
    store.save_scheduler_native_id(
        "c",
        &first.job.trigger,
        &attempt,
        "cached-thread",
        "cached-token",
        true,
    )?;
    let shown = Shown {
        bot: Some("cached-bot".into()),
        works: [("cached-work".into(), "work-revision".into())].into(),
        memory: Some([("cached-memory".into(), "memory-revision".into())].into()),
        dynamic: Some("cached-context".into()),
        session_generation: first.session_generation,
        ..Shown::default()
    };
    store.record_manager_turn(
        &attempt,
        "fixture",
        "cached-thread",
        Some("old-turn"),
        &shown,
    )?;
    store.finish_job(
        &net.a,
        &net.network,
        "c",
        &first.job.trigger,
        &attempt,
        JobOutcome::Completed,
        "done",
        4,
    )?;
    say(&mut store, net, "switch-next", true)?;
    let mut claimed = store
        .claim_next_in_session(&net.a, &net.network, &key, "boot", &config, &limits)?
        .context("resumed turn")?;
    assert_eq!(claimed.previous_native_id.as_deref(), Some("cached-thread"));
    assert_eq!(claimed.shown.bot.as_deref(), Some("cached-bot"));
    assert!(!claimed.covers.as_ref().context("cached covers")?.is_empty());
    let attempt = claimed.job.state.attempt().context("attempt")?.id.clone();
    let runtime = TurnRuntime {
        provider: Provider::Codex,
        model: Some("selected-before-backup".into()),
        reasoning_effort: Some("medium".into()),
        generation: "before-backup".into(),
        session_generation: claimed.session_generation,
        delivery_id: attempt.clone(),
        stage: ProviderStage::Authentication,
        execution: TurnExecution::NotStarted,
        attempted_at_ms: 5,
        switched_from: None,
        switch_reason: None,
    };
    claimed.job = store.record_turn_runtime("c", &claimed.job.trigger, &attempt, &runtime)?;
    Ok((store, claimed, config))
}

fn scheduler_switch_rows(store: &Store) -> Result<Vec<String>> {
    let mut rows = Vec::new();
    for table in ["scheduler_jobs", "scheduler_sessions", "pending_checks"] {
        let mut statement = store
            .connection
            .prepare(&format!("SELECT * FROM {table}"))?;
        let count = statement.column_count();
        for row in statement.query_map([], |row| {
            (0..count)
                .map(|index| row.get::<_, rusqlite::types::Value>(index))
                .collect::<rusqlite::Result<Vec<_>>>()
        })? {
            rows.push(format!("{table}: {:?}", row?));
        }
    }
    rows.sort();
    Ok(rows)
}

#[test]
fn backup_switch_fences_the_cached_session_without_readmitting_the_job() -> Result<()> {
    use crate::scheduling::{RuntimeSelection, SessionConfig};
    let net = net()?;
    let (mut store, before, old) = switchable_scheduler_session(&net)?;
    let attempt = before.job.state.attempt().context("attempt")?;
    let jobs = store.scheduler_jobs("c")?.len();
    let basis = store.version_vector(&net.network)?;
    let config = SessionConfig {
        stable_fingerprint: "stable-after-backup".into(),
        runtime_fingerprint: "runtime-after-backup".into(),
        provider: Provider::Cursor,
    };
    let switched =
        store.switch_scheduler_session("c", &before.job.trigger, &attempt.id, &config)?;
    assert_eq!(switched.job.trigger, before.job.trigger);
    assert_eq!(switched.job.basis, before.job.basis);
    assert_eq!(switched.job.message_id, before.job.message_id);
    assert_eq!(
        switched.job.state.attempt().context("switched attempt")?.id,
        attempt.id
    );
    assert_eq!(
        switched
            .job
            .state
            .attempt()
            .context("switched attempt")?
            .permission_version,
        attempt.permission_version + 1
    );
    assert_eq!(switched.session_generation, before.session_generation + 1);
    assert_eq!(switched.handoff_revision, before.handoff_revision + 1);
    assert_eq!(switched.handoff_source, before.job.basis);
    assert!(
        switched.previous_native_id.is_none()
            && switched.previous_token.is_none()
            && switched.covers.is_none()
    );
    assert_eq!(
        switched.previous_runtime,
        Some(RuntimeSelection {
            provider: Provider::Codex,
            model: Some("selected-before-backup".into()),
            reasoning_effort: Some("medium".into()),
        })
    );
    let invalidation = switched.invalidation.as_ref().context("invalidation")?;
    assert_eq!(invalidation.reason, "provider_changed");
    assert_eq!(invalidation.source_provider, Some(old.provider));
    assert_eq!(invalidation.target_provider, config.provider);
    assert_eq!(
        invalidation.old_stable_fingerprint.as_deref(),
        Some(old.stable_fingerprint.as_str())
    );
    assert_eq!(
        invalidation.new_stable_fingerprint,
        config.stable_fingerprint
    );
    assert_eq!(
        invalidation.old_runtime_fingerprint.as_deref(),
        Some(old.runtime_fingerprint.as_str())
    );
    assert_eq!(
        invalidation.new_runtime_fingerprint,
        config.runtime_fingerprint
    );
    assert_eq!(
        invalidation.source_basis,
        before.covers.clone().context("old covers")?
    );
    assert_eq!(invalidation.generation, switched.session_generation);
    assert_eq!(
        switched.shown.session_lineage.len(),
        before.shown.session_lineage.len() + 1
    );
    let lineage = switched.shown.session_lineage.last().context("lineage")?;
    assert_eq!(lineage.source_basis, invalidation.source_basis);
    assert_eq!(lineage.generation, before.session_generation);
    assert_eq!(lineage.stable_fingerprint, old.stable_fingerprint);
    assert_eq!(lineage.runtime_fingerprint, old.runtime_fingerprint);
    assert_eq!(lineage.provider, old.provider);
    assert_eq!(lineage.handoff_revision, invalidation.handoff_revision);
    assert_eq!(
        switched.shown,
        Shown {
            session_generation: switched.session_generation,
            session_lineage: switched.shown.session_lineage.clone(),
            ..Shown::default()
        }
    );
    assert_eq!(memory_test_shown(&store)?, switched.shown);
    let persisted: (Option<String>, Option<String>, String, String, String, String, u64, u64) = store.connection.query_row(
        "SELECT native_id,native_token,covers,stable_fingerprint,runtime_fingerprint,provider,permission_version,session_generation FROM scheduler_sessions WHERE conversation_id='c' AND scope='main'",
        [],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?, row.get(5)?, row.get(6)?, row.get(7)?)),
    )?;
    assert_eq!(
        (persisted.0, persisted.1, persisted.2.as_str()),
        (None, None, "{}")
    );
    assert_eq!(
        (persisted.3, persisted.4, persisted.5),
        (
            config.stable_fingerprint,
            config.runtime_fingerprint,
            enum_text(&config.provider)
        )
    );
    assert_eq!(
        (persisted.6, persisted.7),
        (attempt.permission_version + 1, switched.session_generation)
    );
    assert_eq!(
        store
            .scheduler_job("c", &before.job.trigger)?
            .context("saved job")?,
        switched.job
    );
    assert_eq!(store.scheduler_jobs("c")?.len(), jobs);
    assert_eq!(store.version_vector(&net.network)?, basis);
    Ok(())
}

#[test]
fn rejected_backup_switches_leave_jobs_sessions_and_checks_unchanged() -> Result<()> {
    use crate::scheduling::{SessionConfig, SessionScope, TurnExecution};
    use vibeface_bridge::failure::ProviderStage;
    for (case, expected) in [
        ("task", "only allowed for human turns"),
        ("started", "started execution"),
        ("unknown", "started execution"),
        ("pending", "side effects"),
        ("tools", "side effects"),
        ("partial", "emitted reply text"),
        ("attempt", "attempt does not match"),
        ("inactive", "no longer active"),
        ("missing-runtime", "turn runtime missing"),
        ("unchanged", "configuration did not change"),
    ] {
        let net = net()?;
        let (mut store, mut claimed, old) = switchable_scheduler_session(&net)?;
        let mut config = SessionConfig {
            stable_fingerprint: "stable-backup".into(),
            runtime_fingerprint: "runtime-backup".into(),
            provider: Provider::Cursor,
        };
        if case == "task" {
            let task = isolated_task(&mut store, &net, "backup-task")?;
            let mut runtime = claimed.job.runtime.clone().context("runtime")?;
            claimed = isolated_claim(
                &mut store,
                &net,
                SessionScope::Task {
                    work_id: task.work_id,
                },
                task.resolved.provider,
                crate::config::SchedulingLimits::default(),
            )?
            .context("task claim")?;
            runtime.provider = task.resolved.provider;
            runtime.model = task.resolved.model;
            runtime.reasoning_effort = None;
            runtime.session_generation = claimed.session_generation;
            runtime.delivery_id = claimed
                .job
                .state
                .attempt()
                .context("task attempt")?
                .id
                .clone();
            store.record_turn_runtime("c", &claimed.job.trigger, &runtime.delivery_id, &runtime)?;
        }
        let trigger = claimed.job.trigger.clone();
        let attempt = claimed.job.state.attempt().context("attempt")?.id.clone();
        match case {
            "started" | "unknown" => {
                store.update_turn_runtime(
                    "c",
                    &trigger,
                    &attempt,
                    ProviderStage::Run,
                    if case == "started" {
                        TurnExecution::Started
                    } else {
                        TurnExecution::Unknown
                    },
                )?;
            }
            "pending" => {
                assert_eq!(store.connection.execute("INSERT INTO pending_checks(attempt_id,conversation_id,trigger,detail,created_at_ms) VALUES(?,'c',?,'unknown provider result',6)", params![attempt, trigger])?, 1);
            }
            "tools" => {
                store.record_manager_tool_call("c", &trigger, &attempt)?;
            }
            "partial" => {
                assert_eq!(store.connection.execute("UPDATE scheduler_jobs SET partial_text='visible partial' WHERE conversation_id='c' AND trigger=?", [&trigger])?, 1);
            }
            "inactive" => {
                assert_eq!(store.connection.execute("UPDATE scheduler_sessions SET active=NULL WHERE conversation_id='c' AND scope='main'", [])?, 1);
            }
            "missing-runtime" => {
                assert_eq!(store.connection.execute("UPDATE scheduler_jobs SET runtime=NULL WHERE conversation_id='c' AND trigger=?", [&trigger])?, 1);
            }
            "unchanged" => {
                config = old;
            }
            _ => {}
        }
        let before = scheduler_switch_rows(&store)?;
        let job = store.scheduler_job("c", &trigger)?.context("saved job")?;
        let basis = store.version_vector(&net.network)?;
        let result = store.switch_scheduler_session(
            "c",
            &trigger,
            if case == "attempt" {
                "wrong-attempt"
            } else {
                &attempt
            },
            &config,
        );
        let error = result.expect_err(case);
        assert!(error.to_string().contains(expected), "{case}: {error}");
        assert_eq!(scheduler_switch_rows(&store)?, before, "{case}");
        assert_eq!(
            store.scheduler_job("c", &trigger)?.context("saved job")?,
            job,
            "{case}"
        );
        assert_eq!(store.version_vector(&net.network)?, basis, "{case}");
    }
    Ok(())
}
