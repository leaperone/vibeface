use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{Arc, Mutex},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use tokio::sync::watch;
use vibeface_bridge::agent::{
    AgentFailure, AgentRunError, NativeStatus, SdkBridge, SdkTurn, StartEvidence,
    ToolCallbackConfig,
};
use vibeface_bridge::failure::FailureKind;

fn starts(log: &Path) -> Vec<u32> {
    std::fs::read_to_string(log)
        .unwrap_or_default()
        .lines()
        .map(|line| line.parse().unwrap())
        .collect()
}

fn signal(group: u32, signal: &str) -> bool {
    Command::new("/bin/kill")
        .args([signal, "--", &format!("-{group}")])
        .stderr(Stdio::null())
        .status()
        .unwrap()
        .success()
}

async fn gone(group: u32) -> bool {
    for _ in 0..50 {
        if !signal(group, "-0") {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    false
}

async fn wait_sends(log: &Path, prefix: &str, expected: usize) {
    for _ in 0..500 {
        let seen = std::fs::read_to_string(log)
            .unwrap_or_default()
            .lines()
            .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
            .filter(|line| {
                line["text"]
                    .as_str()
                    .is_some_and(|text| text.starts_with(prefix))
            })
            .count();
        if seen == expected {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("expected {expected} concurrent fixture sends starting with {prefix}");
}

async fn reply(
    bridge: &SdkBridge,
    turn: &SdkTurn,
    prompt: &str,
) -> (Result<String, AgentRunError>, Duration) {
    let started = Instant::now();
    let result = bridge
        .run(
            turn,
            prompt,
            None,
            |_| async { Ok(()) },
            watch::channel(false).1,
            |_| async { Ok(()) },
        )
        .await
        .map(|result| result.output);
    (result, started.elapsed())
}

#[tokio::test]
async fn one_bridge_serves_consecutive_turns_and_is_replaced_when_it_dies() {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let root = std::env::temp_dir().join(format!("vibeface-resident-bridge-{nanos}"));
    std::fs::create_dir_all(&root).unwrap();
    let log = root.join("starts.log");
    let pidfile = root.join("cursor-sdk-bridge.pid");
    let startup_ms: u64 = std::env::var("FAKE_BRIDGE_STARTUP_MS")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(1500);
    let executable =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/fake-cursor-sdk-bridge");
    let bridge = SdkBridge::new(
        BTreeMap::from([("FAKE_BRIDGE_STARTUP_MS".into(), startup_ms.to_string())]),
        ToolCallbackConfig {
            url: "http://127.0.0.1:9".into(),
            auth_token: "fixture-callback-token-".repeat(2),
        },
        Some(pidfile.clone()),
    )
    .unwrap();
    let recorded = Arc::new(Mutex::new(Vec::new()));
    let recorder = recorded.clone();
    let mut turn = SdkTurn {
        owner: "bot:network:conversation".into(),
        executable: executable.clone(),
        workspace: root.join("workspace"),
        local_store_root: root.join("cursor-instance"),
        api_key: "fixture-api-key".into(),
        env: BTreeMap::from([
            ("FAKE_BRIDGE_LOG".into(), log.display().to_string()),
            (
                "FAKE_BRIDGE_SEND_LOG".into(),
                root.join("sends.log").display().to_string(),
            ),
        ]),
        model: "fixture-model".into(),
        timeout_secs: 10,
        custom_tools: BTreeMap::new(),
        callback_token: Some("fixture-attempt-token-".repeat(2)),
        process_started: Some(Arc::new(move |group| {
            recorder.lock().unwrap().push(group);
            Box::pin(async { Ok(()) })
        })),
        run_started: None,
    };

    let (first, first_took) = reply(&bridge, &turn, "first").await;
    let (second, second_took) = reply(&bridge, &turn, "second").await;
    eprintln!("startup delay {startup_ms}ms");
    eprintln!("turn 1 (cold start): {first_took:?}");
    eprintln!("turn 2 (resident bridge): {second_took:?}");
    assert_eq!(first.unwrap(), "done: first");
    assert_eq!(second.unwrap(), "done: second");
    let bridge_group = starts(&log)[0];
    assert_eq!(
        starts(&log),
        vec![bridge_group],
        "both turns used one bridge"
    );
    assert_eq!(*recorded.lock().unwrap(), vec![bridge_group, bridge_group]);
    assert!(second_took < Duration::from_millis(startup_ms / 2));

    assert!(signal(bridge_group, "-KILL"));
    assert!(gone(bridge_group).await);
    let (third, third_took) = reply(&bridge, &turn, "third").await;
    eprintln!("turn 3 (bridge killed before it): {third_took:?}");
    assert_eq!(third.unwrap(), "done: third");
    let restarted = starts(&log)[1];
    assert_eq!(starts(&log), vec![bridge_group, restarted]);
    assert_ne!(restarted, bridge_group);

    turn.timeout_secs = 1;
    let (hung, _) = reply(&bridge, &turn, "hang").await;
    let hung = hung.unwrap_err();
    assert_eq!(hung.outcome, AgentFailure::Unknown);
    assert_eq!(hung.failure.kind, FailureKind::TimedOut);
    assert!(
        gone(restarted).await,
        "a bridge left with an unknown turn ends after it"
    );
    turn.timeout_secs = 10;
    let (fourth, _) = reply(&bridge, &turn, "fourth").await;
    assert_eq!(fourth.unwrap(), "done: fourth");
    let last = starts(&log)[2];
    assert_eq!(
        std::fs::read_to_string(&pidfile).unwrap(),
        format!("{last} {}\n", executable.display())
    );

    turn.env
        .insert("FIXTURE_ENGINE_VAR".into(), "changed".into());
    let (fifth, _) = reply(&bridge, &turn, "fifth").await;
    assert_eq!(fifth.unwrap(), "done: fifth");
    let relaunched = starts(&log)[3];
    assert_ne!(
        relaunched, last,
        "changed engine variables start a new bridge"
    );
    assert!(gone(last).await, "the bridge with the old variables ends");
    let last = relaunched;

    let sends_before = std::fs::read_to_string(root.join("sends.log")).unwrap();
    let before_send = bridge
        .run(
            &turn,
            "must-not-send",
            None,
            |_| async { anyhow::bail!("fixture persist rejected before Send") },
            watch::channel(false).1,
            |_| async { Ok(()) },
        )
        .await
        .err()
        .expect("persist callback refuses the run");
    assert_eq!(before_send.start, StartEvidence::NotStarted);
    assert!(
        before_send.session_id.is_some(),
        "a created agent alone is not a started model turn"
    );
    assert_eq!(
        std::fs::read_to_string(root.join("sends.log")).unwrap(),
        sends_before
    );

    bridge.stop().await;
    assert!(gone(last).await);
    assert!(!pidfile.exists());
    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn owners_have_private_bridges_and_one_crash_does_not_stop_the_other_fifteen() {
    let root = std::env::temp_dir().join(format!(
        "vibeface-cursor-owners-{}",
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&root).unwrap();
    let starts_log = root.join("starts.log");
    let stores_log = root.join("stores.log");
    let sends_log = root.join("sends.log");
    let gates = root.join("gates");
    std::fs::create_dir(&gates).unwrap();
    let pidfile = root.join("cursor-sdk-bridge.pid");
    let executable =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/fake-cursor-sdk-bridge");
    let bridge = Arc::new(
        SdkBridge::new(
            BTreeMap::from([("FAKE_BRIDGE_DELAY_MS".into(), "100".into())]),
            ToolCallbackConfig {
                url: "http://127.0.0.1:9".into(),
                auth_token: "fixture-callback-token-".repeat(2),
            },
            Some(pidfile.clone()),
        )
        .unwrap(),
    );
    let recorded = Arc::new(Mutex::new(Vec::<(String, u32)>::new()));
    let mut turns = Vec::new();
    for index in 0..16 {
        let owner = format!("bot:network:{index}");
        let recorder = recorded.clone();
        let owner_for_callback = owner.clone();
        turns.push(SdkTurn {
            owner,
            executable: executable.clone(),
            workspace: root.join(format!("workspace-{index}")),
            local_store_root: root.join(format!("instance-{index}")),
            api_key: "fixture-api-key".into(),
            env: BTreeMap::from([
                ("FAKE_BRIDGE_LOG".into(), starts_log.display().to_string()),
                (
                    "FAKE_BRIDGE_STORE_LOG".into(),
                    stores_log.display().to_string(),
                ),
                (
                    "FAKE_BRIDGE_SEND_LOG".into(),
                    sends_log.display().to_string(),
                ),
                ("FAKE_BRIDGE_GATE_DIR".into(), gates.display().to_string()),
            ]),
            model: "fixture-model".into(),
            timeout_secs: 10,
            custom_tools: BTreeMap::new(),
            callback_token: Some(format!(
                "fixture-attempt-token-{index:02}-{}",
                "x".repeat(32)
            )),
            process_started: Some(Arc::new(move |group| {
                recorder
                    .lock()
                    .unwrap()
                    .push((owner_for_callback.clone(), group));
                Box::pin(async { Ok(()) })
            })),
            run_started: None,
        });
    }
    let first: Vec<_> = turns
        .iter()
        .cloned()
        .enumerate()
        .map(|(index, turn)| {
            let bridge = bridge.clone();
            tokio::spawn(async move { reply(&bridge, &turn, &format!("first-{index}")).await })
        })
        .collect();
    wait_sends(&sends_log, "first-", 16).await;
    std::fs::write(gates.join("first"), b"").unwrap();
    let first = futures_util::future::join_all(first).await;
    for (index, result) in first.into_iter().enumerate() {
        assert_eq!(result.unwrap().0.unwrap(), format!("done: first-{index}"));
    }
    let groups: std::collections::BTreeSet<_> = recorded
        .lock()
        .unwrap()
        .iter()
        .map(|(_, group)| *group)
        .collect();
    assert_eq!(groups.len(), 16, "each owner starts its own process group");
    let stores: std::collections::BTreeSet<String> = std::fs::read_to_string(&stores_log)
        .unwrap()
        .lines()
        .map(|line| {
            serde_json::from_str::<serde_json::Value>(line).unwrap()["rootDir"]
                .as_str()
                .unwrap()
                .to_owned()
        })
        .collect();
    assert_eq!(stores.len(), 16, "each owner gets a private JSONL root");
    let instance_prefix = root.join("instance-").display().to_string();
    assert!(
        stores
            .iter()
            .all(|store| !store.contains(".sdk") && store.starts_with(&instance_prefix))
    );

    let crash_owner = "bot:network:0";
    let crash_group = recorded
        .lock()
        .unwrap()
        .iter()
        .find_map(|(owner, group)| (owner == crash_owner).then_some(*group))
        .unwrap();
    let mut second = Vec::new();
    for (index, turn) in turns.iter().cloned().enumerate() {
        let prompt = if index == 0 {
            "hang".to_owned()
        } else {
            format!("second-{index}")
        };
        let bridge = bridge.clone();
        second.push(tokio::spawn(
            async move { reply(&bridge, &turn, &prompt).await },
        ));
    }
    wait_sends(&sends_log, "second-", 15).await;
    wait_sends(&sends_log, "hang", 1).await;
    assert!(signal(crash_group, "-KILL"));
    std::fs::write(gates.join("second"), b"").unwrap();
    let second: Vec<_> = futures_util::future::join_all(second)
        .await
        .into_iter()
        .map(|result| result.unwrap())
        .collect();
    assert!(second[0].0.is_err());
    for (index, (result, _)) in second.into_iter().enumerate().skip(1) {
        assert_eq!(result.unwrap(), format!("done: second-{index}"));
    }
    let (cancel_one, cancelled) = watch::channel(false);
    let third: Vec<_> = turns
        .into_iter()
        .enumerate()
        .map(|(index, turn)| {
            let bridge = bridge.clone();
            let cancel = if index == 0 {
                cancelled.clone()
            } else {
                watch::channel(false).1
            };
            tokio::spawn(async move {
                let prompt = if index == 0 {
                    "cancelhang-0".to_owned()
                } else {
                    format!("cancel-{index}")
                };
                bridge
                    .run(
                        &turn,
                        &prompt,
                        None,
                        |_| async { Ok(()) },
                        cancel,
                        |_| async { Ok(()) },
                    )
                    .await
            })
        })
        .collect();
    wait_sends(&sends_log, "cancel", 16).await;
    cancel_one.send(true).unwrap();
    std::fs::write(gates.join("cancel"), b"").unwrap();
    let third = futures_util::future::join_all(third).await;
    for (index, result) in third.into_iter().enumerate() {
        if index == 0 {
            let error = match result.unwrap() {
                Err(error) => error,
                Ok(_) => panic!("cancelled owner unexpectedly completed"),
            };
            assert_eq!(error.outcome, AgentFailure::Unknown);
            assert_ne!(
                error.start,
                StartEvidence::NotStarted,
                "a sent Cursor run never restores its charge"
            );
        } else {
            assert_eq!(
                result.unwrap().unwrap().output,
                format!("done: cancel-{index}")
            );
        }
    }
    assert_eq!(
        std::fs::read_to_string(&sends_log).unwrap().lines().count(),
        48,
        "no crash or cancellation replays a Send"
    );
    bridge.stop().await;
    assert!(!pidfile.exists());
    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn one_owner_serializes_attempt_callbacks_and_resumes_its_private_store_after_restart() {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let root = std::env::temp_dir().join(format!("vibeface-cursor-resume-{nanos}"));
    let gates = root.join("gates");
    std::fs::create_dir_all(&gates).unwrap();
    let sends = root.join("sends.log");
    let starts_log = root.join("starts.log");
    let bridge = Arc::new(
        SdkBridge::new(
            BTreeMap::new(),
            ToolCallbackConfig {
                url: "http://127.0.0.1:9".into(),
                auth_token: "fixture-callback-token-".repeat(2),
            },
            Some(root.join("bridge.pid")),
        )
        .unwrap(),
    );
    let token_a = "fixture-first-attempt-token-".repeat(2);
    let token_b = "fixture-second-attempt-token-".repeat(2);
    let turn = SdkTurn {
        owner: "bot:network:resume".into(),
        executable: PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/fake-cursor-sdk-bridge"),
        workspace: root.join("workspace"),
        local_store_root: root.join("private-instance"),
        api_key: "fixture-api-key".into(),
        env: BTreeMap::from([
            ("FAKE_BRIDGE_LOG".into(), starts_log.display().to_string()),
            ("FAKE_BRIDGE_SEND_LOG".into(), sends.display().to_string()),
            ("FAKE_BRIDGE_GATE_DIR".into(), gates.display().to_string()),
        ]),
        model: "fixture-model".into(),
        timeout_secs: 10,
        custom_tools: BTreeMap::new(),
        callback_token: Some(token_a.clone()),
        process_started: None,
        run_started: None,
    };
    let first_bridge = bridge.clone();
    let first_turn = turn.clone();
    let (native_tx, native_rx) = tokio::sync::oneshot::channel();
    let before_send_log = sends.clone();
    let first = tokio::spawn(async move {
        first_bridge
            .run(
                &first_turn,
                "first-memory",
                None,
                move |native| async move {
                    assert!(
                        !before_send_log.exists(),
                        "native id is durable before Send"
                    );
                    native_tx.send(native).unwrap();
                    Ok(())
                },
                watch::channel(false).1,
                |_| async { Ok(()) },
            )
            .await
    });
    let native = native_rx.await.unwrap();
    wait_sends(&sends, "first-", 1).await;
    let mut second_turn = turn.clone();
    second_turn.callback_token = Some(token_b.clone());
    let second_bridge = bridge.clone();
    let second_native = native.clone();
    let second = tokio::spawn(async move {
        second_bridge
            .run(
                &second_turn,
                "second-memory",
                Some(&second_native),
                |_| async { Ok(()) },
                watch::channel(false).1,
                |_| async { Ok(()) },
            )
            .await
    });
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(
        std::fs::read_to_string(&sends).unwrap().lines().count(),
        1,
        "the second attempt cannot enter Send while the owner's first turn is live"
    );
    std::fs::write(gates.join("first"), b"").unwrap();
    assert_eq!(first.await.unwrap().unwrap().output, "done: first-memory");
    wait_sends(&sends, "second-", 1).await;
    std::fs::write(gates.join("second"), b"").unwrap();
    assert_eq!(
        second.await.unwrap().unwrap().session_id.as_deref(),
        Some(native.as_str())
    );
    let callbacks: Vec<_> = std::fs::read_to_string(&sends)
        .unwrap()
        .lines()
        .map(|line| {
            serde_json::from_str::<serde_json::Value>(line).unwrap()["callback"]
                .as_str()
                .unwrap()
                .to_owned()
        })
        .collect();
    assert_eq!(callbacks, vec![token_a, token_b]);
    let first_group = starts(&starts_log)[0];
    assert!(signal(first_group, "-KILL"));
    assert!(gone(first_group).await);
    std::fs::write(gates.join("recall"), b"").unwrap();
    let result = bridge
        .run(
            &turn,
            "recall",
            Some(&native),
            |_| async { Ok(()) },
            watch::channel(false).1,
            |_| async { Ok(()) },
        )
        .await
        .unwrap();
    assert_eq!(result.session_id.as_deref(), Some(native.as_str()));
    assert_eq!(result.output, "remembered: second-memory");
    assert_eq!(
        starts(&starts_log).len(),
        2,
        "resume starts an independent replacement process"
    );
    assert!(!turn.workspace.join(".sdk").exists());
    bridge.stop().await;
    for group in starts(&starts_log) {
        assert!(gone(group).await);
    }
    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn run_id_callback_precedes_progress_and_known_run_probe_reads_terminal_state() {
    let root = std::env::temp_dir().join(format!(
        "vibeface-cursor-run-query-{}",
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&root).unwrap();
    let executable =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/fake-cursor-sdk-bridge");
    let bridge = SdkBridge::new(
        BTreeMap::new(),
        ToolCallbackConfig {
            url: "http://127.0.0.1:9".into(),
            auth_token: "fixture-callback-token-".repeat(2),
        },
        Some(root.join("bridge.pid")),
    )
    .unwrap();
    let order = Arc::new(Mutex::new(Vec::<String>::new()));
    let run_id = Arc::new(Mutex::new(None));
    let starts_log = root.join("starts.log");
    let callback_order = order.clone();
    let callback_run = run_id.clone();
    let turn = SdkTurn {
        owner: "work:network:conversation:root".into(),
        executable,
        workspace: root.join("workspace"),
        local_store_root: root.join("private-instance"),
        api_key: "fixture-api-key".into(),
        env: BTreeMap::from([
            (
                "FAKE_BRIDGE_GET_RUN_STATUS".into(),
                "RUN_LIFECYCLE_STATUS_FINISHED".into(),
            ),
            ("FAKE_BRIDGE_REJECT_RESUME".into(), "1".into()),
            ("FAKE_BRIDGE_LOG".into(), starts_log.display().to_string()),
        ]),
        model: "fixture-model".into(),
        timeout_secs: 10,
        custom_tools: BTreeMap::new(),
        callback_token: None,
        process_started: None,
        run_started: Some(Arc::new(move |id| {
            callback_order.lock().unwrap().push("run".into());
            *callback_run.lock().unwrap() = Some(id);
            Box::pin(async { Ok(()) })
        })),
    };
    let progress_order = order.clone();
    let result = bridge
        .run(
            &turn,
            "query me",
            None,
            |_| async { Ok(()) },
            watch::channel(false).1,
            move |_| {
                progress_order.lock().unwrap().push("progress".into());
                async { Ok(()) }
            },
        )
        .await
        .unwrap();
    let run_id = run_id.lock().unwrap().clone().unwrap();
    assert_eq!(result.output, "done: query me");
    let order = order.lock().unwrap().clone();
    assert_eq!(order.first().map(String::as_str), Some("run"));
    bridge.retire_all().await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    let native = bridge
        .probe(&turn, result.session_id.as_deref().unwrap(), &run_id)
        .await
        .unwrap();
    assert_eq!(native.status, NativeStatus::Completed);
    assert_eq!(native.output.as_deref(), Some("done: query me"));
    assert!(
        starts(&starts_log).len() >= 2,
        "probe restarted the resident bridge"
    );
    bridge
        .cancel(&turn, result.session_id.as_deref().unwrap(), &run_id)
        .await
        .unwrap();
    bridge.stop().await;
    std::fs::remove_dir_all(root).unwrap();
}
