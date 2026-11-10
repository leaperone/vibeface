//! Real controlled ACP children, with private config/data and the production prompt journal.
use anyhow::{Context, Result};
use serde_json::Value;
use std::{
    collections::BTreeMap,
    path::PathBuf,
    sync::{Arc, Mutex},
};
use tokio::sync::{Notify, watch};
use vibeface_bridge::agent::{
    AgentFailure, CursorAgent, CursorAuthentication, CursorCatalog, CursorTurn, PromptStatus,
    StartEvidence,
};

fn fixture() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(if cfg!(windows) {
        "tests/fixtures/fake-cursor-agent.cmd"
    } else {
        "tests/fixtures/fake-cursor-agent"
    })
}
fn root() -> Result<PathBuf> {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../.artifacts")
        .join(format!("cursor-acp-{}", uuid::Uuid::new_v4()));
    vibeface_bridge::platform::privatefs::create_dir(&root)?;
    root.canonicalize().map_err(Into::into)
}
fn turn(root: &std::path::Path, client: &str) -> CursorTurn {
    CursorTurn {
        owner: "fixture-owner".into(),
        client_id: client.into(),
        executable: fixture(),
        workspace: root.into(),
        local_store_root: root.join("owner"),
        env: BTreeMap::from([
            (
                "FAKE_BRIDGE_LOG".into(),
                root.join("processes.jsonl").display().to_string(),
            ),
            (
                "FAKE_BRIDGE_SEND_LOG".into(),
                root.join("prompts.jsonl").display().to_string(),
            ),
            (
                "FAKE_ACP_REPLIES".into(),
                root.join("replies.jsonl").display().to_string(),
            ),
            (
                "FAKE_ACP_JOURNAL_ROOT".into(),
                root.join("owner").display().to_string(),
            ),
        ]),
        model: "fixture-model".into(),
        timeout_secs: 10,
        custom_tools: BTreeMap::new(),
        callback_token: None,
        process_started: None,
        run_started: None,
    }
}
fn agent(root: &std::path::Path) -> Result<CursorAgent> {
    CursorAgent::new(
        BTreeMap::new(),
        "http://127.0.0.1:9".into(),
        Some(root.join("agent.pid")),
    )
}
fn lines(path: &std::path::Path) -> Vec<Value> {
    std::fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

#[tokio::test]
async fn official_subscription_probe_observes_authentication_and_models_without_a_prompt()
-> Result<()> {
    let root = root()?;
    let agent = agent(&root)?;
    let observation = agent
        .provider_observation(&fixture(), &root, &root.join("probe"), &BTreeMap::new())
        .await?;
    assert_eq!(
        observation.authentication,
        CursorAuthentication::Authenticated
    );
    assert!(
        matches!(observation.catalog,CursorCatalog::Available{models} if models.iter().any(|m|m.id=="composer-2"))
    );
    let rejected = agent
        .provider_observation(
            &fixture(),
            &root,
            &root.join("signed-out"),
            &BTreeMap::from([("FAKE_BRIDGE_ME".into(), "401".into())]),
        )
        .await?;
    assert_eq!(
        rejected.authentication,
        CursorAuthentication::SignInRequired
    );
    assert_eq!(rejected.catalog, CursorCatalog::Unavailable);
    assert!(!root.join("prompts.jsonl").exists());
    agent.stop().await;
    std::fs::remove_dir_all(root)?;
    Ok(())
}

#[tokio::test]
async fn prompt_receipts_are_durable_before_send_and_terminal_before_reply_and_never_replay()
-> Result<()> {
    let root = root()?;
    let agent = agent(&root)?;
    let mut turn = turn(&root, "first");
    let starts = Arc::new(Mutex::new(Vec::new()));
    let recorded = starts.clone();
    let journal = turn.local_store_root.join("prompts");
    turn.run_started = Some(Arc::new(move |id| {
        let recorded = recorded.clone();
        let journal = journal.clone();
        Box::pin(async move {
            let receipt: Value =
                serde_json::from_slice(&std::fs::read(journal.join(format!("{id}.json")))?)?;
            assert_eq!(receipt["status"]["state"], "running");
            recorded.lock().unwrap().push(id);
            Ok(())
        })
    }));
    let result = agent
        .run(
            &turn,
            "first answer",
            None,
            |_| async { Ok(()) },
            watch::channel(false).1,
            |_| async { Ok(()) },
        )
        .await?;
    assert_eq!(result.output, "done: first answer");
    let session = result.session_id.context("session")?;
    let receipt = starts.lock().unwrap()[0].clone();
    assert_eq!(
        agent.probe(&turn, &session, &receipt).await?.status,
        PromptStatus::Completed
    );
    let repeat = agent
        .run(
            &turn,
            "first answer",
            None,
            |_| async { Ok(()) },
            watch::channel(false).1,
            |_| async { Ok(()) },
        )
        .await?;
    assert_eq!(repeat.output, "done: first answer");
    assert_eq!(lines(&root.join("prompts.jsonl")).len(), 1);
    assert!(
        agent
            .run(
                &turn,
                "changed intention",
                None,
                |_| async { Ok(()) },
                watch::channel(false).1,
                |_| async { Ok(()) }
            )
            .await
            .is_err()
    );
    turn.client_id = "continuation".into();
    let updates = Arc::new(Mutex::new(Vec::new()));
    let seen = updates.clone();
    let continued = agent
        .run(
            &turn,
            "new answer",
            Some(&session),
            |_| async { Ok(()) },
            watch::channel(false).1,
            move |update| {
                seen.lock().unwrap().push(update);
                async { Ok(()) }
            },
        )
        .await?;
    assert_eq!(continued.output, "done: new answer");
    assert!(!serde_json::to_string(&*updates.lock().unwrap())?.contains("HISTORICAL"));
    assert!(
        lines(&root.join("replies.jsonl"))
            .iter()
            .any(|r| r["id"] == "history-permission"
                && r["result"]["outcome"]["optionId"] == "reject")
    );
    assert_eq!(lines(&root.join("prompts.jsonl")).len(), 2);
    assert!(vibeface_bridge::platform::privatefs::is_private(
        &turn.local_store_root.join("config")
    )?);
    agent.stop().await;
    assert!(!root.join("agent.pid").exists());
    std::fs::remove_dir_all(root)?;
    Ok(())
}

#[tokio::test]
async fn cancellation_requires_the_original_prompt_terminal_and_disconnect_stays_unknown_after_restart()
-> Result<()> {
    let root = root()?;
    let agent = Arc::new(agent(&root)?);
    let mut turn = turn(&root, "cancel");
    let ready = Arc::new(Notify::new());
    let started = ready.clone();
    let locator = Arc::new(Mutex::new(None));
    let saved = locator.clone();
    turn.run_started = Some(Arc::new(move |id| {
        let started = started.clone();
        let saved = saved.clone();
        Box::pin(async move {
            *saved.lock().unwrap() = Some(id);
            started.notify_one();
            Ok(())
        })
    }));
    let session = Arc::new(Mutex::new(None));
    let saved_session = session.clone();
    let worker = agent.clone();
    let running = turn.clone();
    let pending = tokio::spawn(async move {
        worker
            .run(
                &running,
                "hang here",
                None,
                move |id| {
                    *saved_session.lock().unwrap() = Some(id);
                    async { Ok(()) }
                },
                watch::channel(false).1,
                |_| async { Ok(()) },
            )
            .await
    });
    tokio::time::timeout(std::time::Duration::from_secs(10), ready.notified()).await?;
    let session = session.lock().unwrap().clone().context("session saved")?;
    let id = locator.lock().unwrap().clone().context("receipt saved")?;
    agent.cancel(&turn, &session, &id).await?;
    let error = pending.await?.err().context("provider cancellation")?;
    assert_eq!(error.outcome, AgentFailure::Cancelled);
    assert_eq!(error.start, StartEvidence::Started);
    assert_eq!(
        agent.probe(&turn, &session, &id).await?.status,
        PromptStatus::Cancelled
    );
    turn.client_id = "unknown".into();
    let error = agent
        .run(
            &turn,
            "disconnect",
            None,
            |_| async { Ok(()) },
            watch::channel(false).1,
            |_| async { Ok(()) },
        )
        .await
        .err()
        .context("unknown completion")?;
    assert_eq!(error.outcome, AgentFailure::Unknown);
    let session = error.session_id.context("unknown session")?;
    let id = locator.lock().unwrap().clone().context("unknown receipt")?;
    agent.stop().await;
    let restarted = CursorAgent::new(
        BTreeMap::new(),
        "http://127.0.0.1:9".into(),
        Some(root.join("agent.pid")),
    )?;
    assert_eq!(
        restarted.probe(&turn, &session, &id).await?.status,
        PromptStatus::Unknown
    );
    let before = lines(&root.join("prompts.jsonl")).len();
    let error = restarted
        .run(
            &turn,
            "disconnect",
            None,
            |_| async { Ok(()) },
            watch::channel(false).1,
            |_| async { Ok(()) },
        )
        .await
        .err()
        .context("no replay")?;
    assert_eq!(error.outcome, AgentFailure::Unknown);
    assert_eq!(lines(&root.join("prompts.jsonl")).len(), before);
    restarted.stop().await;
    std::fs::remove_dir_all(root)?;
    Ok(())
}

#[tokio::test]
async fn scoped_permissions_and_blocking_extensions_always_receive_safe_responses() -> Result<()> {
    let root = root()?;
    let agent = agent(&root)?;
    let turn = turn(&root, "interactions");
    let mut updates = Vec::new();
    let result = agent
        .run(
            &turn,
            "interactions",
            None,
            |_| async { Ok(()) },
            watch::channel(false).1,
            |update| {
                updates.push(update);
                async { Ok(()) }
            },
        )
        .await?;
    assert_eq!(result.output, "done: interactions");
    assert_eq!(
        updates
            .iter()
            .filter(|update| update["status"] == "interaction_declined")
            .count(),
        2
    );
    let replies = lines(&root.join("replies.jsonl"));
    for (id, selected) in [("permission", "allow"), ("unknown-permission", "reject")] {
        assert!(
            replies
                .iter()
                .any(|r| r["id"] == id && r["result"]["outcome"]["optionId"] == selected)
        );
    }
    assert!(
        replies
            .iter()
            .any(|r| r["id"] == "ask" && r["result"]["outcome"]["outcome"] == "cancelled")
    );
    assert!(
        replies
            .iter()
            .any(|r| r["id"] == "plan" && r["result"]["outcome"]["outcome"] == "rejected")
    );
    assert!(
        replies
            .iter()
            .any(|r| r["id"] == "unknown" && r["error"]["code"] == -32601)
    );
    let error = agent
        .run(
            &turn,
            "different",
            Some("foreign-personal-session"),
            |_| async { Ok(()) },
            watch::channel(false).1,
            |_| async { Ok(()) },
        )
        .await;
    assert!(error.is_err());
    assert_eq!(lines(&root.join("prompts.jsonl")).len(), 1);
    agent.stop().await;
    std::fs::remove_dir_all(root)?;
    Ok(())
}

#[tokio::test]
async fn cursor_stream_and_journal_redact_fragmented_credentials() -> Result<()> {
    let root = root()?;
    let agent = agent(&root)?;
    let mut turn = turn(&root, "redaction");
    turn.env
        .insert("FIXTURE_SECRET".into(), "synthetic-sensitive-token".into());
    turn.env
        .insert("FIXTURE_SHORT_SECRET".into(), "synthetic-sensitive".into());
    turn.env.insert("FAKE_ACP_CHUNK_SIZE".into(), "1".into());
    let updates = Arc::new(Mutex::new(Vec::new()));
    let seen = updates.clone();
    let result = agent
        .run(
            &turn,
            "synthetic-sensitive-token",
            None,
            |_| async { Ok(()) },
            watch::channel(false).1,
            move |value| {
                seen.lock().unwrap().push(value);
                async { Ok(()) }
            },
        )
        .await?;
    assert_eq!(result.output, "done: [redacted]");
    let joined = updates
        .lock()
        .unwrap()
        .iter()
        .filter_map(|r| r["text"].as_str())
        .collect::<String>();
    assert!(!joined.contains("synthetic-sensitive-token"));
    assert!(!joined.contains("-token"));
    {
        let updates = updates.lock().unwrap();
        let snapshot = updates.last().context("final reply snapshot")?;
        assert_eq!(snapshot["type"], "text_snapshot");
        assert_eq!(snapshot["text"], result.output);
        assert!(
            updates
                .iter()
                .filter(|update| matches!(
                    update["type"].as_str(),
                    Some("text_delta" | "text_snapshot")
                ))
                .all(|update| update["item_id"] == snapshot["item_id"]),
            "the final snapshot replaces the streamed item instead of rendering a second reply"
        );
    }
    for entry in std::fs::read_dir(turn.local_store_root.join("prompts"))? {
        assert!(!std::fs::read_to_string(entry?.path())?.contains("synthetic-sensitive-token"));
    }
    agent.stop().await;
    std::fs::remove_dir_all(root)?;
    Ok(())
}

#[tokio::test]
async fn a_signed_out_or_partial_status_never_starts_acp_even_with_explicit_auth() -> Result<()> {
    let root = root()?;
    let agent = agent(&root)?;
    for state in ["FAKE_STATUS_SIGNED_OUT", "FAKE_STATUS_PARTIAL"] {
        let env = BTreeMap::from([
            (state.into(), "1".into()),
            (
                "FAKE_BRIDGE_LOG".into(),
                root.join("acp-starts").display().to_string(),
            ),
        ]);
        let result = agent
            .provider_observation(&fixture(), &root, &root.join(state), &env)
            .await?;
        assert_eq!(result.authentication, CursorAuthentication::SignInRequired);
        assert!(!result.launchable);
        assert!(
            !root.join("acp-starts").exists(),
            "status exit 0 does not authorize starting ACP"
        );
    }
    for name in [
        "CURSOR_API_KEY",
        "CURSOR_AUTH_TOKEN",
        "AGENT_CLI_CREDENTIAL_STORE",
    ] {
        let env = BTreeMap::from([
            (name.into(), "synthetic-explicit-auth".into()),
            ("FAKE_STATUS_SIGNED_OUT".into(), "1".into()),
        ]);
        let result = agent
            .provider_observation(&fixture(), &root, &root.join(name), &env)
            .await?;
        assert_eq!(result.authentication, CursorAuthentication::SignInRequired);
        assert!(!result.launchable);
        assert!(!root.join("acp-starts").exists());
        let mut turn = turn(&root, name);
        turn.env = env;
        assert!(
            agent
                .run(
                    &turn,
                    "must not run",
                    None,
                    |_| async { Ok(()) },
                    watch::channel(false).1,
                    |_| async { Ok(()) }
                )
                .await
                .is_err()
        );
        assert!(!root.join("prompts.jsonl").exists());
    }
    agent.stop().await;
    std::fs::remove_dir_all(root)?;
    Ok(())
}

#[tokio::test]
async fn a_process_exit_after_cancel_does_not_confirm_cancellation() -> Result<()> {
    let root = root()?;
    let agent = agent(&root)?;
    let mut turn = turn(&root, "cancel-without-terminal");
    turn.env
        .insert("FAKE_ACP_EXIT_ON_CANCEL".into(), "1".into());
    let (cancel, receiver) = watch::channel(false);
    let locator = Arc::new(Mutex::new(None));
    let saved = locator.clone();
    turn.run_started = Some(Arc::new(move |id| {
        let saved = saved.clone();
        let cancel = cancel.clone();
        Box::pin(async move {
            *saved.lock().unwrap() = Some(id);
            cancel.send(true)?;
            Ok(())
        })
    }));
    let error = agent
        .run(
            &turn,
            "hang without terminal",
            None,
            |_| async { Ok(()) },
            receiver,
            |_| async { Ok(()) },
        )
        .await
        .err()
        .context("unknown cancellation")?;
    assert_eq!(error.outcome, AgentFailure::Unknown);
    assert_eq!(error.start, StartEvidence::Started);
    let session = error.session_id.context("session")?;
    let id = locator.lock().unwrap().clone().context("receipt")?;
    assert_eq!(
        agent.probe(&turn, &session, &id).await?.status,
        PromptStatus::Unknown
    );
    agent.stop().await;
    std::fs::remove_dir_all(root)?;
    Ok(())
}

#[tokio::test]
async fn mcp_reconnect_uses_the_current_attempt_bearer_without_persisting_it() -> Result<()> {
    use sha2::{Digest, Sha256};
    use vibeface_bridge::agent::CustomToolConfig;
    let root = root()?;
    let agent = agent(&root)?;
    let mut turn = turn(&root, "tool-first");
    turn.env.insert(
        "FAKE_ACP_SESSION_LOG".into(),
        root.join("sessions.jsonl").display().to_string(),
    );
    turn.custom_tools.insert(
        "tool_search".into(),
        CustomToolConfig {
            description: None,
            input_schema: serde_json::json!({"type":"object"}),
            output_schema: None,
        },
    );
    let first = "a".repeat(64);
    let next = "b".repeat(64);
    turn.callback_token = Some(first.clone());
    let reply = agent
        .run(
            &turn,
            "first tools",
            None,
            |_| async { Ok(()) },
            watch::channel(false).1,
            |_| async { Ok(()) },
        )
        .await?;
    turn.client_id = "tool-next".into();
    turn.callback_token = Some(next.clone());
    agent
        .run(
            &turn,
            "next tools",
            reply.session_id.as_deref(),
            |_| async { Ok(()) },
            watch::channel(false).1,
            |_| async { Ok(()) },
        )
        .await?;
    let sessions = lines(&root.join("sessions.jsonl"));
    for (row, token) in sessions.iter().zip([&first, &next]) {
        assert_eq!(
            row["mcpServers"][0]["headers"][0]["value"],
            format!("{:x}", Sha256::digest(format!("Bearer {token}").as_bytes()))
        );
    }
    for path in [
        root.join("sessions.jsonl"),
        turn.local_store_root.join("prompts"),
    ] {
        let bytes = if path.is_dir() {
            std::fs::read_dir(path)?
                .map(|entry| std::fs::read_to_string(entry.unwrap().path()).unwrap())
                .collect::<String>()
        } else {
            std::fs::read_to_string(path)?
        };
        assert!(!bytes.contains(&first) && !bytes.contains(&next));
    }
    agent.stop().await;
    std::fs::remove_dir_all(root)?;
    Ok(())
}

#[tokio::test]
async fn orphan_recovery_checks_the_recorded_process_before_ending_its_tree() -> Result<()> {
    let root = root()?;
    let agent = Arc::new(agent(&root)?);
    let mut turn = turn(&root, "orphan");
    let ready = Arc::new(Notify::new());
    let started = ready.clone();
    turn.run_started = Some(Arc::new(move |_| {
        let started = started.clone();
        Box::pin(async move {
            started.notify_one();
            Ok(())
        })
    }));
    let worker = agent.clone();
    let running = turn.clone();
    let pending = tokio::spawn(async move {
        worker
            .run(
                &running,
                "hang as orphan",
                None,
                |_| async { Ok(()) },
                watch::channel(false).1,
                |_| async { Ok(()) },
            )
            .await
    });
    tokio::time::timeout(std::time::Duration::from_secs(10), ready.notified()).await?;
    let file = root.join("agent.pid");
    #[cfg(unix)]
    {
        let recorded = std::fs::read(&file)?;
        let mut wrong: Value = serde_json::from_slice(&recorded)?;
        wrong[0]["identity"]["value"] = serde_json::json!("another-process-start");
        vibeface_bridge::platform::privatefs::write_atomic(&file, &serde_json::to_vec(&wrong)?)?;
        CursorAgent::kill_orphan(&file);
        assert!(
            !pending.is_finished(),
            "a stale identifier cannot end another CLI process"
        );
        vibeface_bridge::platform::privatefs::write_atomic(&file, &recorded)?;
    }
    CursorAgent::kill_orphan(&file);
    let error = pending
        .await?
        .err()
        .context("orphan completion stays unknown")?;
    assert_eq!(error.outcome, AgentFailure::Unknown);
    assert_eq!(lines(&root.join("prompts.jsonl")).len(), 1);
    agent.stop().await;
    std::fs::remove_dir_all(root)?;
    Ok(())
}
