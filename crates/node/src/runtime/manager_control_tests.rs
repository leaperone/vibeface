use super::*;
use tokio::sync::watch;

async fn running_manager() -> Result<(PathBuf, Arc<Runtime>, Access, watch::Receiver<bool>)> {
    let dir = tests::fixture()?;
    let node = Arc::new(Runtime::open(&dir).await?);
    let network = tests::conversation(&node).await?;
    let target = node.identity.node_id()?;
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

fn operation(access: &Access) -> OperationRequest {
    OperationRequest {
        operation_id: "original-operation".into(),
        network_id: access.network_id.clone(),
        conversation_id: "chat".into(),
        task_id: Some("original-task".into()),
        target_node: access.manager.as_ref().unwrap().target_node.clone(),
        session_id: "original-session".into(),
        expected_revision: 3,
        basis_heads: vec!["original-basis".into()],
        action: json!({"type":"send","text":"continue"}),
        resources: vec![],
        binding: None,
        manager_authority: None,
    }
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

#[tokio::test]
async fn management_operation_identity_is_immutable_across_retries() -> Result<()> {
    let (dir, node, access, _) = running_manager().await?;
    let request = operation(&access);
    let key = format!(
        "manager-tools:v1:{}",
        access.manager.as_ref().unwrap().attempt_id
    );
    let mut store = node.store.lock().await;
    Runtime::record_manager_operation(&mut store, &access, &request)?;
    let original = store.local_value(&access.network_id, &key)?;
    Runtime::record_manager_operation(&mut store, &access, &request)?;
    for field in ["target", "session", "task"] {
        let mut changed = request.clone();
        match field {
            "target" => changed.target_node = "different-executor".into(),
            "session" => changed.session_id = "different-session".into(),
            "task" => changed.task_id = Some("different-task".into()),
            _ => unreachable!(),
        }
        assert!(Runtime::record_manager_operation(&mut store, &access, &changed).is_err());
        assert_eq!(store.local_value(&access.network_id, &key)?, original);
    }
    drop(store);
    node.finish_manager_attempt(&access.manager.as_ref().unwrap().attempt_id)
        .await;
    drop(node);
    std::fs::remove_dir_all(dir)?;
    Ok(())
}

#[test]
fn management_resolution_rejects_a_receipt_from_another_executor_with_the_same_id() -> Result<()> {
    let access = Access {
        network_id: "network".into(),
        human: false,
        conversation_id: Some("chat".into()),
        manager: Some(ManagerAccess {
            attempt_id: "attempt".into(),
            trigger: "trigger".into(),
            target_node: "original-executor".into(),
            read_only: false,
            followup_version: None,
        }),
    };
    let original = operation(&access);
    let reference = json!({
        "operation_id":original.operation_id,
        "target_node":original.target_node,
        "session_id":original.session_id,
        "task_id":original.task_id,
    });
    let mut record = OperationRecord {
        request: original.clone(),
        state: OperationState::Succeeded,
        result: json!({"done":true}),
    };
    Runtime::verify_manager_operation("network", "chat", &reference, &original, &record)?;
    record.request.target_node = "different-executor".into();
    assert_eq!(record.request.operation_id, original.operation_id);
    assert!(
        Runtime::verify_manager_operation("network", "chat", &reference, &original, &record)
            .is_err()
    );
    Ok(())
}
