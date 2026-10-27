use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{Arc, Mutex},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use tokio::sync::watch;
use vibeface_bridge::agent::{AgentFailure, AgentRunError, SdkBridge, SdkTurn, ToolCallbackConfig};
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
        executable: executable.clone(),
        workspace: root.join("workspace"),
        api_key: "fixture-api-key".into(),
        env: BTreeMap::from([("FAKE_BRIDGE_LOG".into(), log.display().to_string())]),
        model: "fixture-model".into(),
        timeout_secs: 10,
        custom_tools: BTreeMap::new(),
        process_started: Some(Arc::new(move |group| {
            recorder.lock().unwrap().push(group);
            Box::pin(async { Ok(()) })
        })),
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

    bridge.stop().await;
    assert!(gone(last).await);
    assert!(!pidfile.exists());
    std::fs::remove_dir_all(root).unwrap();
}
