use super::*;
use crate::connection::tests::{receive, send};
use vibeface_secure::membership::Genesis;

#[test]
fn recovery_selection_keeps_client_permission_choices_and_rejects_rewritten_members() -> Result<()>
{
    let old_manager = Identity::generate()?;
    let ordinary = Identity::generate()?;
    let node = Identity::generate()?;
    let omitted = Identity::generate()?;
    let caller = Identity::generate()?;
    let allowed = vec![
        Member::client(&old_manager, "old manager", true)?,
        Member::client(&ordinary, "ordinary", false)?,
        Member::node(&node, "connected Node")?,
        Member::node(&omitted, "omitted Node")?,
    ];
    let manager = Member::client(&caller, "new manager", true)?;
    let selected = vec![
        Member::client(&old_manager, "old manager", false)?,
        Member::client(&ordinary, "ordinary", true)?,
        allowed[2].clone(),
    ];
    let mut expected = selected.clone();
    expected.push(manager.clone());
    assert_eq!(
        recovery_members(
            &allowed,
            Some(&selected),
            &node.device_id(),
            manager.clone()
        )?,
        expected
    );
    let mut expected_default = allowed.clone();
    expected_default.push(manager.clone());
    assert_eq!(
        recovery_members(&allowed, None, &node.device_id(), manager.clone())?,
        expected_default
    );

    let mut unknown = selected.clone();
    unknown[0].device_id = Identity::generate()?.device_id();
    let mut renamed = selected.clone();
    renamed[0].name = "changed name".into();
    let mut changed_kind = selected.clone();
    changed_kind[0].kind = IdentityKind::Node;
    let mut node_manager = selected.clone();
    node_manager[2].manage = true;
    let mut duplicate = selected.clone();
    duplicate.push(selected[0].clone());
    let mut injected_caller = selected.clone();
    injected_caller.push(manager.clone());
    for (reason, invalid) in [
        ("unknown device", unknown),
        ("changed name", renamed),
        ("changed kind", changed_kind),
        ("Node manager", node_manager),
        ("duplicate device", duplicate),
        ("missing connected Node", selected[..2].to_vec()),
        ("injected new manager", injected_caller),
    ] {
        assert!(
            recovery_members(&allowed, Some(&invalid), &node.device_id(), manager.clone()).is_err(),
            "accepted {reason}"
        );
    }
    Ok(())
}

#[tokio::test]
async fn review_on_an_empty_node_lists_candidates_without_preparing_recovery() -> Result<()> {
    let dir = std::env::temp_dir().join(format!("review-recovery-{}", uuid::Uuid::new_v4()));
    let key = ClientKey::create(&dir, "client password")?;
    let root = Identity::generate()?;
    let manager = Identity::generate()?;
    let old_node = Identity::generate()?;
    let new_node = Identity::generate()?;
    let genesis = Genesis::create(
        &root,
        Member::client(&manager, "manager", true)?,
        Member::node(&old_node, "old Node")?,
    )?;
    let old_file = dir.join("recovery.json");
    write_recovery_file(
        &old_file,
        &RecoveryMaterial {
            version: 1,
            network_id: genesis.network_id.clone(),
            root_secret_key: Zeroizing::new(root.secret_hex()),
            genesis: genesis.clone(),
            checkpoints: vec![],
            events: vec![],
        },
        "recovery password",
    )?;
    let original = std::fs::read(&old_file)?;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let entry = Entry {
        origin: format!("http://{}", listener.local_addr()?),
        node_id: new_node.device_id(),
    };
    let task = tokio::spawn(async move {
        let (mut socket, mut channel) = rotation_tests::accept(&listener, &new_node).await?;
        let request = receive(&mut socket, &mut channel).await?;
        assert_eq!(request["method"], "info");
        send(
            &mut socket,
            &mut channel,
            json!({"id":request["id"],"result":{"initialized":false,"name":"new Node"}}),
        )
        .await?;
        socket.close(None).await?;
        Ok::<_, anyhow::Error>(())
    });
    let review = review_recovery(
        &key,
        entry.clone(),
        &old_file,
        "recovery password",
        RecoveryOptions {
            name: "rescuer",
            capability: None,
            members: None,
        },
    )
    .await?;
    task.await??;
    let mut expected = genesis.members();
    expected.push(Member::new(
        &entry.node_id,
        IdentityKind::Node,
        "new Node",
        false,
    )?);
    assert_eq!(review.members, expected);
    assert_eq!(review.network_id, genesis.network_id);
    assert_eq!(review.required_node, entry.node_id);
    assert_eq!(
        review.new_manager,
        Member::client(&key.identity, "rescuer", true)?
    );
    assert!(!IdentityPlan::exists(&key));
    assert!(!dir.join("identity-operation.lock").exists());
    assert_eq!(std::fs::read(&old_file)?, original);
    assert!(key.network_id().is_none());
    std::fs::remove_dir_all(dir)?;
    Ok(())
}

#[tokio::test]
async fn selected_recovery_retires_omitted_nodes_and_resumes_the_exact_unknown_plan() -> Result<()>
{
    let dir = std::env::temp_dir().join(format!("selected-recovery-{}", uuid::Uuid::new_v4()));
    let mut key = ClientKey::create(&dir, "client password")?;
    let root = Identity::generate()?;
    let old_manager = Identity::generate()?;
    let ordinary = Identity::generate()?;
    let node = Identity::generate()?;
    let omitted = Identity::generate()?;
    let revoked = Identity::generate()?;
    let mut state = NetworkState::new(Genesis::create(
        &root,
        Member::client(&old_manager, "old manager", true)?,
        Member::node(&node, "connected Node")?,
    )?)?;
    for member in [
        Member::client(&ordinary, "ordinary", false)?,
        Member::node(&omitted, "omitted Node")?,
        Member::node(&revoked, "revoked Node")?,
    ] {
        state.append(GovernanceEvent::grant(
            &old_manager,
            &state.project()?,
            member,
        )?)?;
    }
    let revoked_cutoff = Revocation::new(&revoked.device_id(), Some((3, &"33".repeat(32))))?;
    state.append(GovernanceEvent::revoke(
        &old_manager,
        &state.project()?,
        revoked_cutoff.clone(),
    )?)?;
    let old_file = dir.join("old-recovery.json");
    let material = RecoveryMaterial {
        version: 1,
        network_id: state.genesis.network_id.clone(),
        root_secret_key: Zeroizing::new(root.secret_hex()),
        genesis: state.genesis.clone(),
        checkpoints: state.checkpoints.clone(),
        events: state.events.clone(),
    };
    write_recovery_file(&old_file, &material, "recovery password")?;
    let selected = vec![
        Member::client(&old_manager, "old manager", false)?,
        Member::client(&ordinary, "ordinary", true)?,
        Member::node(&node, "connected Node")?,
    ];
    let mut expected = selected.clone();
    expected.push(Member::client(&key.identity, "rescuer", true)?);
    expected.sort_by(|a, b| a.device_id.cmp(&b.device_id));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let entry = Entry {
        origin: format!("http://{}", listener.local_addr()?),
        node_id: node.device_id(),
    };
    let omitted_id = omitted.device_id();
    let server_hash = "55".repeat(32);
    let server_expected = expected.clone();
    let server_network = state.genesis.network_id.clone();
    let task = tokio::spawn(async move {
        let before_review = serde_json::to_value(&state)?;
        for _ in 0..2 {
            let (mut socket, mut channel) = rotation_tests::accept(&listener, &node).await?;
            let request = receive(&mut socket, &mut channel).await?;
            assert_eq!(request["method"], "info");
            send(
                &mut socket,
                &mut channel,
                json!({"id":request["id"],"result":{"initialized":true}}),
            )
            .await?;
            let request = receive(&mut socket, &mut channel).await?;
            assert_eq!(request["method"], "recovery_state");
            send(
                &mut socket,
                &mut channel,
                json!({"id":request["id"],"result":{
                    "state":state,"now_ms":now_ms(),"heads":{omitted_id.clone():[5,server_hash]}
                }}),
            )
            .await?;
            socket.close(None).await?;
            assert_eq!(serde_json::to_value(&state)?, before_review);
        }

        let (mut socket, mut channel) = rotation_tests::accept(&listener, &node).await?;
        let request = receive(&mut socket, &mut channel).await?;
        assert_eq!(request["method"], "recover");
        let received: NetworkState = serde_json::from_value(request["params"].clone())?;
        let checkpoint = received.checkpoints.last().context("recovery checkpoint")?;
        let directory = checkpoint
            .directory
            .as_ref()
            .context("recovery directory")?;
        assert_eq!(directory.members, server_expected);
        let mut expected_retired = vec![
            revoked_cutoff,
            Revocation::new(&omitted_id, Some((5, &"55".repeat(32))))?,
        ];
        expected_retired.sort_by(|a, b| a.device_id.cmp(&b.device_id));
        assert_eq!(directory.retired, expected_retired);
        assert_eq!(
            received.project_at(checkpoint.effective_at_ms())?.members,
            server_expected
        );
        let hash = checkpoint.hash()?;
        socket.close(None).await?;

        // The first receipt is unavailable. Retrying the saved Unknown plan must only read.
        for known in [false, true] {
            let (mut socket, mut channel) = rotation_tests::accept(&listener, &node).await?;
            let request = receive(&mut socket, &mut channel).await?;
            assert_eq!(
                request["method"], "authenticate",
                "recovery was resubmitted"
            );
            let result = if known {
                json!({"network_id":server_network,"recovery":{"status":"waiting","checkpoint_hash":hash}})
            } else {
                json!({})
            };
            send(
                &mut socket,
                &mut channel,
                json!({"id":request["id"],"result":result}),
            )
            .await?;
        }
        Ok::<_, anyhow::Error>(())
    });
    let replacement = dir.join("new-recovery.json");
    let original_file = std::fs::read(&old_file)?;
    let review = review_recovery(
        &key,
        entry.clone(),
        &old_file,
        "recovery password",
        RecoveryOptions {
            name: "rescuer",
            capability: None,
            members: None,
        },
    )
    .await?;
    let mut expected_candidates = vec![
        Member::client(&old_manager, "old manager", true)?,
        Member::client(&ordinary, "ordinary", false)?,
        Member::new(&entry.node_id, IdentityKind::Node, "connected Node", false)?,
        Member::node(&omitted, "omitted Node")?,
    ];
    expected_candidates.sort_by(|a, b| a.device_id.cmp(&b.device_id));
    assert_eq!(review.members, expected_candidates);
    assert_eq!(review.network_id, material.network_id);
    assert_eq!(review.required_node, entry.node_id);
    assert_eq!(
        review.new_manager,
        Member::client(&key.identity, "rescuer", true)?
    );
    assert!(!replacement.exists());
    assert!(!IdentityPlan::exists(&key));
    assert_eq!(std::fs::read(&old_file)?, original_file);
    let mut plan = prepare_recovery(
        &key,
        entry.clone(),
        &old_file,
        "recovery password",
        &replacement,
        RecoveryOptions {
            name: "rescuer",
            capability: None,
            members: Some(&selected),
        },
    )
    .await?;
    assert_eq!(plan.recovery_members(), Some(expected.as_slice()));
    let backup = std::fs::read(&replacement)?;
    let replacement_material = read_recovery_file(&replacement, "recovery password")?;
    assert_ne!(
        Identity::from_secret_hex(&replacement_material.root_secret_key)?.device_id(),
        root.device_id()
    );
    assert!(plan.submit(&mut key).await.is_err());
    assert_eq!(plan.state(), IdentityOperationState::Unknown);
    drop(plan);
    let journal = std::fs::read(dir.join("identity-operation.json"))?;
    std::fs::remove_file(&replacement)?;
    let saved_review = IdentityPlan::recovery_review(&key, "recovery password", &entry)?;
    let mut saved_selection = selected.clone();
    saved_selection.sort_by(|a, b| a.device_id.cmp(&b.device_id));
    assert_eq!(saved_review.members, saved_selection);
    assert_eq!(
        saved_review.new_manager,
        Member::client(&key.identity, "rescuer", true)?
    );
    assert!(
        !replacement.exists(),
        "review must not recreate a missing backup"
    );
    assert_eq!(std::fs::read(dir.join("identity-operation.json"))?, journal);
    let mut resumed = IdentityPlan::load(&key, "recovery password")?;
    resumed.validate_recovery_intent(&entry, &replacement, "rescuer", None)?;
    let mut reordered = selected.clone();
    reordered.reverse();
    resumed.validate_recovery_intent(&entry, &replacement, "rescuer", Some(&reordered))?;
    let mut different_permission = selected.clone();
    different_permission[0].manage = true;
    for invalid in [different_permission, selected[1..].to_vec()] {
        assert!(
            resumed
                .validate_recovery_intent(&entry, &replacement, "rescuer", Some(&invalid))
                .is_err()
        );
    }
    assert!(
        resumed
            .validate_recovery_intent(&entry, &replacement, "another name", None)
            .is_err()
    );
    assert!(
        resumed
            .validate_recovery_intent(&entry, &dir.join("another-file.json"), "rescuer", None)
            .is_err()
    );
    let mut wrong_entry = entry.clone();
    wrong_entry.node_id = omitted.device_id();
    assert!(
        resumed
            .validate_recovery_intent(&wrong_entry, &replacement, "rescuer", None)
            .is_err()
    );
    assert_eq!(std::fs::read(dir.join("identity-operation.json"))?, journal);
    assert_eq!(std::fs::read(&replacement)?, backup);
    assert_eq!(resumed.recovery_members(), Some(expected.as_slice()));
    resumed.submit(&mut key).await?;
    assert_eq!(resumed.state(), IdentityOperationState::Recorded);
    task.await??;
    assert_eq!(std::fs::read(&replacement)?, backup);
    drop(resumed);
    std::fs::remove_dir_all(dir)?;
    Ok(())
}
