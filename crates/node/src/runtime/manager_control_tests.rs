use super::*;
use tokio::sync::watch;

async fn running_manager() -> Result<(PathBuf, Arc<Runtime>, Access, watch::Receiver<bool>)> {
    let dir = tests::fixture()?;
    let node = Arc::new(Runtime::open(&dir).await?);
    let network = tests::conversation(&node).await?;
    let target = node.identity.peer_id()?;
    let (trigger, attempt) = {
        let mut store = node.store.lock().await;
        let (message, _) = store.accept_message(
            &node.identity,
            &network,
            "chat",
            EventBody::MessagePosted {
                message_id: "manager-fence-message".into(),
                role: "user".into(),
                text: "create a task".into(),
                scheduler_node: Some(target.clone()),
            },
            vec![],
            2,
            &target,
            &target,
            false,
        )?;
        let claimed = store
            .claim_next(&network, "chat", &target, "fence-test-boot", "test-sdk")?
            .context("management job was not claimed")?;
        let attempt = claimed
            .job
            .state
            .attempt()
            .context("claimed job has no attempt")?
            .id
            .clone();
        store.mark_started(&network, "chat", &target, &message.hash, &attempt)?;
        (message.hash, attempt)
    };
    let token = "manager-fence-token".to_string();
    let cancel = node
        .register_manager_attempt(
            token.clone(),
            Access {
                network_id: network,
                human: false,
                conversation_id: Some("chat".into()),
                manager: None,
            },
            &attempt,
            &trigger,
            false,
        )
        .await?;
    let access = node
        .agent_tokens
        .lock()
        .await
        .get(&token)
        .context("registered management token missing")?
        .clone();
    Ok((dir, node, access, cancel))
}

#[tokio::test]
async fn queued_manager_cancellation_preserves_the_active_reply_and_other_messages() -> Result<()> {
    let (dir, node, access, cancel) = running_manager().await?;
    let manager = access.manager.as_ref().unwrap();
    let human = Access {
        human: true,
        manager: None,
        ..access.clone()
    };
    let (queued, remaining, original) = {
        let mut store = node.store.lock().await;
        let mut accept = |id: &str| -> Result<String> {
            Ok(store
                .accept_message(
                    &node.identity,
                    &access.network_id,
                    "chat",
                    EventBody::MessagePosted {
                        message_id: id.into(),
                        role: "user".into(),
                        text: id.into(),
                        scheduler_node: Some(manager.target_node.clone()),
                    },
                    vec![],
                    2,
                    &manager.target_node,
                    &manager.target_node,
                    false,
                )?
                .0
                .hash)
        };
        let queued = accept("cancel-this-queued-message")?;
        let remaining = accept("keep-this-queued-message")?;
        let original = store
            .scheduler_job(&access.network_id, "chat", &queued, &manager.target_node)?
            .unwrap();
        (queued, remaining, original)
    };
    let input = json!({"conversation_id":"chat","trigger":queued});
    let receipt = node.cancel_manager(&human, &input).await?;
    assert_eq!(receipt["cancel_requested"], true);
    assert_eq!(receipt["job"]["state"], "cancelled");
    let cancelled: crate::scheduling::SchedulerJob =
        serde_json::from_value(receipt["job"].clone())?;
    assert!(cancelled.state.attempt().is_none());
    assert_eq!(cancelled.message_id, original.message_id);
    assert_eq!(cancelled.basis_heads, original.basis_heads);
    assert_eq!(cancelled.sequence, original.sequence);
    assert_eq!(
        node.cancel_manager(&human, &input).await?["job"],
        receipt["job"]
    );
    assert!(!*cancel.borrow());
    let mut store = node.store.lock().await;
    Runtime::check_manager_permission(&store, &access, true, None)?;
    assert_eq!(store.messages(&access.network_id, "chat")?.len(), 3);
    store.finish_job(
        &node.identity,
        &access.network_id,
        "chat",
        &manager.target_node,
        &manager.trigger,
        &manager.attempt_id,
        crate::scheduling::JobOutcome::Failed("fixture complete".into()),
        vec![],
        3,
    )?;
    assert_eq!(
        store
            .claim_next(
                &access.network_id,
                "chat",
                &manager.target_node,
                "fence-test-boot",
                "test-sdk",
            )?
            .unwrap()
            .job
            .trigger,
        remaining
    );
    drop(store);
    node.finish_manager_attempt(&manager.attempt_id).await;
    drop(node);
    std::fs::remove_dir_all(dir)?;
    Ok(())
}

#[tokio::test]
async fn manager_fence_revokes_persisted_permission_before_draining_admitted_tools() -> Result<()> {
    let (dir, node, access, mut cancel) = running_manager().await?;
    let attempt = access.manager.as_ref().unwrap().attempt_id.clone();
    let lease = node
        .manager_tool_lease(&access, true, None)
        .await?
        .context("management tool lease missing")?;
    let store = node.store.lock().await;
    Runtime::check_manager_permission(&store, &access, true, None)?;
    let fence_node = node.clone();
    let fence = tokio::spawn(async move { fence_node.fence_manager_attempt(&attempt).await });
    tokio::time::timeout(Duration::from_secs(5), cancel.changed()).await??;
    assert!(*cancel.borrow());
    drop(store);

    let store = node.store.lock().await;
    assert!(Runtime::check_manager_permission(&store, &access, true, None).is_err());
    assert!(
        !fence.is_finished(),
        "fence must wait for the admitted tool"
    );
    assert!(node.agent_tokens.lock().await.is_empty());
    drop(store);
    drop(lease);
    tokio::time::timeout(Duration::from_secs(5), fence).await???;

    drop(node);
    std::fs::remove_dir_all(dir)?;
    Ok(())
}
