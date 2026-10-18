use super::*;
use tokio::sync::watch;

pub(super) async fn running_manager() -> Result<(
    crate::identity::test_support::TestNetwork,
    Arc<Runtime>,
    Access,
    watch::Receiver<bool>,
)> {
    let (network, node) = tests::runtime_fixture().await?;
    let session = tests::agent(&network, &node).await?;
    let trigger = session.sdk_session.session_id.clone();
    let target = node.identity.node_id()?;
    let attempt = node
        .store
        .lock()
        .await
        .scheduler_job(network.space.as_str(), "conversation", &trigger, &target)?
        .context("management job missing")?
        .state
        .attempt()
        .context("management attempt missing")?
        .id
        .clone();
    let cancel = node
        .register_manager_attempt(
            "manager-fence-token".into(),
            session.clone(),
            &attempt,
            &trigger,
            false,
        )
        .await?;
    let mut access = node
        .authorize_agent(&session, network.body("context", "conversation"))
        .await?;
    access.manager = Some(node.manager_access(&session).await?);
    Ok((network, node, access, cancel))
}

fn operation(access: &Access) -> OperationRequest {
    OperationRequest {
        operation_id: "original-operation".into(),
        network_id: access.network_id.clone(),
        conversation_id: "conversation".into(),
        task_id: Some("original-task".into()),
        target_node: access.manager.as_ref().unwrap().target_node.clone(),
        session_id: Some("original-session".into()),
        expected_revision: 3,
        basis_heads: vec!["original-basis".into()],
        action: json!({"type":"send","text":"continue"}),
        resources: vec![],
        binding: None,
        manager_authority: None,
    }
}

#[tokio::test]
async fn sdk_multiline_call_id_runs_command_once_and_can_be_looked_up() -> Result<()> {
    let (network, node, access, _cancel) = running_manager().await?;
    let dir = network.directory.clone();
    let call_id =
        "call-4b54f5fe-7ec8-49c3-87cc-4585e0c43006-3\nfc_afd18a25-35f8-99d3-92b1-37b9c066cd76_0";
    let args = json!({
        "command": "printf x >> count; cat count",
        "cwd": network.project.canonicalize()?,
        "target_node": node.identity.node_id()?,
        "timeout_secs": 10,
    });
    let first = node
        .agent_tool(access.clone(), "command", args.clone(), call_id)
        .await?;
    let reference = first["call_ref"]
        .as_str()
        .context("missing call reference")?;
    assert!(!reference.chars().any(char::is_control));
    assert!(reference.len() <= 256);
    assert_eq!(first["operations"][0]["state"], "succeeded", "{first}");
    assert_eq!(first["operations"][0]["receipt"]["result"]["stdout"], "x");

    let repeated = node
        .agent_tool(access.clone(), "command", args.clone(), call_id)
        .await?;
    assert_eq!(repeated["work_ref"], first["work_ref"]);
    let mut explicit = args.clone();
    explicit["call_ref"] = json!(reference);
    let retried = node
        .agent_tool(access.clone(), "command", explicit, "another-sdk-id")
        .await?;
    assert_eq!(retried["work_ref"], first["work_ref"]);
    let looked_up = node
        .agent_tool(
            access.clone(),
            "call_lookup",
            json!({"call_ref": reference}),
            "lookup",
        )
        .await?;
    assert_eq!(looked_up["operations"], first["operations"]);
    assert_eq!(std::fs::read_to_string(network.project.join("count"))?, "x");

    let mut invalid = args;
    invalid["call_ref"] = json!(call_id);
    let error = node
        .agent_tool(access, "command", invalid, "valid-sdk-id")
        .await
        .unwrap_err();
    assert_eq!(
        error.to_string(),
        "a bounded stable call reference is required"
    );
    drop(node);
    std::fs::remove_dir_all(dir)?;
    Ok(())
}

#[tokio::test]
async fn queued_manager_cancellation_preserves_the_active_reply_and_other_messages() -> Result<()> {
    let (network, node, access, cancel) = running_manager().await?;
    let dir = network.directory.clone();
    let manager = access.manager.as_ref().unwrap();
    let enqueue = |id: &str| {
        let mut body = network.body("message", "conversation");
        body["message_id"] = json!(id);
        body["text"] = json!(id);
        body["scheduler_node"] = json!(manager.target_node);
        body["report_results"] = json!(false);
        body
    };
    tests::rpc(&network, &node, enqueue("cancel-this-queued-message")).await?;
    tests::rpc(&network, &node, enqueue("keep-this-queued-message")).await?;
    let (queued, remaining, original) = {
        let store = node.store.lock().await;
        let messages = store.messages(&access.network_id, "conversation")?;
        let queued = messages
            .iter()
            .find(|message| message.message_id == "cancel-this-queued-message")
            .context("queued message missing")?
            .event_hash
            .clone();
        let remaining = messages
            .iter()
            .find(|message| message.message_id == "keep-this-queued-message")
            .context("remaining message missing")?
            .event_hash
            .clone();
        let job = store
            .scheduler_job(
                &access.network_id,
                "conversation",
                &queued,
                &manager.target_node,
            )?
            .context("queued job missing")?;
        (queued, remaining, job)
    };
    let mut input = network.body("cancel_manager", "conversation");
    input["trigger"] = json!(queued);
    input["target_node"] = json!(manager.target_node);
    input["attempt_id"] = Value::Null;
    let human = tests::access(&network, &node, &input).await?;
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
    assert_eq!(store.messages(&access.network_id, "conversation")?.len(), 3);
    let current = store
        .scheduler_job(
            &access.network_id,
            "conversation",
            &manager.trigger,
            &manager.target_node,
        )?
        .context("active job missing")?;
    store.finish_scheduler_locally(
        &access.network_id,
        &current,
        crate::scheduling::JobOutcome::Failed("fixture complete".into()),
    )?;
    let session_ref = vibeface_secure::authorization::SessionRef {
        target: node.audience()?.device,
        session_id: remaining.clone(),
    };
    let descriptor = store.agent_grant(
        &access.network_id,
        "conversation",
        &node.audience()?.service,
        &session_ref,
        &remaining,
        super::now_ms() as u64,
    )?;
    drop(store);
    let next = node
        .claim_manager(&descriptor, &remaining, "fence-test-boot", "test-sdk")
        .await?;
    assert_eq!(next.job.trigger, remaining);
    node.finish_manager_attempt(&manager.attempt_id).await;
    drop(node);
    std::fs::remove_dir_all(dir)?;
    Ok(())
}

#[tokio::test]
async fn manager_fence_revokes_persisted_permission_before_draining_admitted_tools() -> Result<()> {
    let (network, node, access, mut cancel) = running_manager().await?;
    let dir = network.directory.clone();
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
    let (network, node, access, _) = running_manager().await?;
    let dir = network.directory.clone();
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
            "session" => changed.session_id = Some("different-session".into()),
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
    let original = OperationRequest {
        operation_id: "original-operation".into(),
        network_id: "network".into(),
        conversation_id: "conversation".into(),
        task_id: Some("original-task".into()),
        target_node: "original-executor".into(),
        session_id: Some("original-session".into()),
        expected_revision: 3,
        basis_heads: vec!["original-basis".into()],
        action: json!({"type":"send","text":"continue"}),
        resources: vec![],
        binding: None,
        manager_authority: None,
    };
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
    Runtime::verify_manager_operation("network", "conversation", &reference, &original, &record)?;
    record.request.target_node = "different-executor".into();
    assert_eq!(record.request.operation_id, original.operation_id);
    assert!(
        Runtime::verify_manager_operation(
            "network",
            "conversation",
            &reference,
            &original,
            &record
        )
        .is_err()
    );
    Ok(())
}
