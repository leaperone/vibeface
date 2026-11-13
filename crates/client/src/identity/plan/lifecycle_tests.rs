use super::super::rotation_tests::accept;
use super::*;
use crate::connection::tests::{receive, send};

fn saved_plan_fixture(
    kind: IdentityPlanKind,
) -> Result<(PathBuf, ClientKey, IdentityPlan, NetworkState, Identity)> {
    let dir = std::env::temp_dir().join(format!("saved-identity-{}", uuid::Uuid::new_v4()));
    let key = ClientKey::create(&dir.join("key"), "client passphrase")?;
    let root = Identity::generate()?;
    let next = Identity::generate()?;
    let node = Identity::generate()?;
    let mut state = NetworkState::new(vibeface_secure::membership::Genesis::create(
        &root,
        Member::client(&key.identity, "client", true)?,
        Member::node(&node, "node")?,
    )?)?;
    let now = now_ms() as u64;
    let checkpoint = match kind {
        IdentityPlanKind::Creation => None,
        IdentityPlanKind::Recovery => Some(RootCheckpoint::recovery(
            &state,
            &root,
            &next.device_id(),
            state.project_at(now)?.members,
            vec![],
            now,
        )?),
        IdentityPlanKind::KeyRotation => Some(RootCheckpoint::rotation(
            &state,
            &root,
            &next.device_id(),
            now,
        )?),
    };
    if let Some(checkpoint) = checkpoint {
        state.recover_at(checkpoint, now)?;
    }
    let material = RecoveryMaterial {
        version: 1,
        network_id: state.genesis.network_id.clone(),
        root_secret_key: Zeroizing::new(if kind == IdentityPlanKind::Creation {
            root.secret_hex()
        } else {
            next.secret_hex()
        }),
        genesis: state.genesis.clone(),
        checkpoints: state.checkpoints.clone(),
        events: vec![],
    };
    let params = if kind == IdentityPlanKind::Creation {
        json!({"genesis":state.genesis,"capability":"fixture-one-time-capability"})
    } else {
        serde_json::to_value(&state)?
    };
    let plan = IdentityPlan::prepare(
        &key,
        Entry {
            origin: "http://127.0.0.1:1".into(),
            node_id: node.device_id(),
        },
        &dir.join("recovery.json"),
        &material,
        "recovery password",
        params,
    )?;
    Ok((dir, key, plan, state, node))
}

#[test]
fn inspect_is_public_metadata_and_does_not_repair_or_create_a_lease() -> Result<()> {
    let (dir, key, plan, state, _) = saved_plan_fixture(IdentityPlanKind::Creation)?;
    let path = plan.backup_path().to_owned();
    let hash = plan.backup_sha256();
    let target = plan.plan.entry.clone();
    let journal = std::fs::read(key.dir.join(PLAN_FILE))?;
    assert!(IdentityPlan::inspect(&key, "recovery password")?.is_some());
    assert!(IdentityPlan::discard_saved(&key, "recovery password").is_err());
    drop(plan);
    std::fs::remove_file(&path)?;
    let lock_path = key.dir.join("identity-operation.lock");
    std::fs::remove_file(&lock_path)?;
    let info = IdentityPlan::inspect(&key, "recovery password")?.context("saved metadata")?;
    assert_eq!(
        serde_json::to_value(info)?,
        json!({
            "network_id":state.genesis.network_id,
            "client_id":key.device_id(),
            "target":target,
            "backup_path":path,
            "backup_sha256":hash,
            "state":"prepared",
            "kind":"creation",
            "members":state.genesis.members(),
        })
    );
    assert!(IdentityPlan::inspect(&key, "wrong password").is_err());
    assert!(!path.exists());
    assert!(!lock_path.exists());
    assert_eq!(std::fs::read(key.dir.join(PLAN_FILE))?, journal);
    IdentityPlan::discard_saved(&key, "recovery password")?;
    assert!(
        !path.exists(),
        "explicit discard must not repair a missing backup"
    );
    std::fs::remove_dir_all(dir)?;
    Ok(())
}

#[test]
fn saved_discard_preserves_unknown_and_recorded_until_a_stopped_receipt() -> Result<()> {
    let (dir, mut key, mut plan, state, _) = saved_plan_fixture(IdentityPlanKind::Recovery)?;
    let backup_path = plan.backup_path().to_owned();
    let backup = std::fs::read(&backup_path)?;
    plan.plan.state = IdentityOperationState::Unknown;
    plan.save()?;
    drop(plan);
    for expected in [
        IdentityOperationState::Unknown,
        IdentityOperationState::Recorded,
    ] {
        let journal = std::fs::read(key.dir.join(PLAN_FILE))?;
        assert_eq!(
            IdentityPlan::inspect(&key, "recovery password")?
                .context("saved plan")?
                .state,
            expected
        );
        assert!(IdentityPlan::discard_saved(&key, "recovery password").is_err());
        assert!(!IdentityPlan::discard_applied(&key, "recovery password")?);
        assert_eq!(std::fs::read(key.dir.join(PLAN_FILE))?, journal);
        assert_eq!(std::fs::read(&backup_path)?, backup);
        if expected == IdentityOperationState::Unknown {
            let mut plan = IdentityPlan::load(&key, "recovery password")?;
            plan.record_receipt(
                &mut key,
                &json!({"network_id":state.genesis.network_id,"state":state,"now_ms":now_ms()}),
            )?;
            assert_eq!(plan.state(), IdentityOperationState::Recorded);
        }
    }
    let mut plan = IdentityPlan::load(&key, "recovery password")?;
    plan.record_receipt(
        &mut key,
        &json!({
            "network_id":state.genesis.network_id,
            "recovery":{"status":"stopped","checkpoint_hash":state.checkpoints[0].hash()?}
        }),
    )?;
    assert_eq!(plan.state(), IdentityOperationState::Stopped);
    drop(plan);
    assert!(!IdentityPlan::discard_applied(&key, "recovery password")?);
    IdentityPlan::discard_saved(&key, "recovery password")?;
    assert!(!IdentityPlan::exists(&key));
    assert_eq!(std::fs::read(&backup_path)?, backup);
    std::fs::remove_dir_all(dir)?;
    Ok(())
}

#[test]
fn applied_receipts_allow_idempotent_cleanup_without_removing_backups() -> Result<()> {
    for kind in [
        IdentityPlanKind::Creation,
        IdentityPlanKind::Recovery,
        IdentityPlanKind::KeyRotation,
    ] {
        let (dir, mut key, plan, state, _) = saved_plan_fixture(kind)?;
        let backup_path = plan.backup_path().to_owned();
        let backup = std::fs::read(&backup_path)?;
        let prepared = std::fs::read(key.dir.join(PLAN_FILE))?;
        assert!(
            IdentityPlan::discard_applied(&key, "recovery password").is_err(),
            "the active lease must protect the original operation"
        );
        drop(plan);
        assert!(!IdentityPlan::discard_applied(&key, "recovery password")?);
        assert_eq!(std::fs::read(key.dir.join(PLAN_FILE))?, prepared);
        let mut plan = IdentityPlan::load(&key, "recovery password")?;
        let applied_at = state
            .checkpoints
            .last()
            .map_or(now_ms() as u64, RootCheckpoint::effective_at_ms);
        plan.record_receipt(
            &mut key,
            &json!({"network_id":state.genesis.network_id,"state":state,"now_ms":applied_at}),
        )?;
        assert_eq!(plan.state(), IdentityOperationState::Applied);
        drop(plan);
        let info = IdentityPlan::inspect(&key, "recovery password")?.context("applied plan")?;
        assert_eq!(info.kind, kind);
        assert_eq!(info.state, IdentityOperationState::Applied);
        assert_eq!(
            info.members.is_none(),
            kind == IdentityPlanKind::KeyRotation
        );
        assert!(IdentityPlan::discard_applied(&key, "recovery password")?);
        assert!(!IdentityPlan::discard_applied(&key, "recovery password")?);
        assert!(IdentityPlan::inspect(&key, "recovery password")?.is_none());
        assert_eq!(std::fs::read(&backup_path)?, backup);
        std::fs::remove_dir_all(dir)?;
    }
    Ok(())
}

#[test]
fn prepared_explicit_discard_preserves_the_original_backup() -> Result<()> {
    let (dir, key, plan, _, _) = saved_plan_fixture(IdentityPlanKind::Creation)?;
    let backup_path = plan.backup_path().to_owned();
    let backup = std::fs::read(&backup_path)?;
    drop(plan);
    IdentityPlan::discard_saved(&key, "recovery password")?;
    assert!(!IdentityPlan::exists(&key));
    assert_eq!(std::fs::read(&backup_path)?, backup);
    assert!(!IdentityPlan::discard_applied(&key, "recovery password")?);
    std::fs::remove_dir_all(dir)?;
    Ok(())
}

#[tokio::test]
async fn applied_creation_does_not_block_read_only_recovery_review() -> Result<()> {
    let (dir, mut key, mut plan, state, node) = saved_plan_fixture(IdentityPlanKind::Creation)?;
    let pending_entry = plan.plan.entry.clone();
    let backup_path = plan.backup_path().to_owned();
    let rejected = review_recovery(
        &key,
        pending_entry,
        &backup_path,
        "recovery password",
        RecoveryOptions {
            name: "client",
            capability: None,
            members: None,
        },
    )
    .await;
    assert!(
        rejected
            .err()
            .context("unresolved plan should block current-state review")?
            .to_string()
            .contains("pending")
    );
    plan.record_receipt(
        &mut key,
        &json!({"network_id":state.genesis.network_id,"state":state,"now_ms":now_ms()}),
    )?;
    assert_eq!(plan.state(), IdentityOperationState::Applied);
    let recovery_file = dir.join("review-recovery.json");
    std::fs::copy(&backup_path, &recovery_file)?;
    drop(plan);
    std::fs::remove_file(&backup_path)?;
    let lock_path = key.dir.join("identity-operation.lock");
    std::fs::remove_file(&lock_path)?;
    let journal = std::fs::read(key.dir.join(PLAN_FILE))?;
    let client_file = std::fs::read(key.dir.join(KEY_FILE))?;
    let recovery_bytes = std::fs::read(&recovery_file)?;
    let expected_node = state.genesis.first_node.clone();
    let expected_network = state.genesis.network_id.clone();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let entry = Entry {
        origin: format!("http://{}", listener.local_addr()?),
        node_id: node.device_id(),
    };
    let task = tokio::spawn(async move {
        let (mut socket, mut channel) = accept(&listener, &node).await?;
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
            json!({"id":request["id"],"result":{"state":state,"now_ms":now_ms(),"heads":{}}}),
        )
        .await?;
        socket.close(None).await?;
        Ok::<_, anyhow::Error>(())
    });
    let review = review_recovery(
        &key,
        entry.clone(),
        &recovery_file,
        "recovery password",
        RecoveryOptions {
            name: "reviewed client",
            capability: None,
            members: None,
        },
    )
    .await?;
    task.await??;
    assert_eq!(review.members, vec![expected_node]);
    assert_eq!(review.network_id, expected_network);
    assert_eq!(review.required_node, entry.node_id);
    assert_eq!(
        review.new_manager,
        Member::client(&key.identity, "reviewed client", true)?
    );
    assert!(!backup_path.exists());
    assert!(!lock_path.exists());
    assert_eq!(std::fs::read(key.dir.join(PLAN_FILE))?, journal);
    assert_eq!(std::fs::read(key.dir.join(KEY_FILE))?, client_file);
    assert_eq!(std::fs::read(&recovery_file)?, recovery_bytes);
    assert_eq!(
        IdentityPlan::inspect(&key, "recovery password")?
            .context("retained journal")?
            .state,
        IdentityOperationState::Applied
    );
    std::fs::remove_dir_all(dir)?;
    Ok(())
}

#[test]
fn applied_creation_survives_a_failed_local_enrollment_write() -> Result<()> {
    let (dir, mut key, mut plan, state, _) = saved_plan_fixture(IdentityPlanKind::Creation)?;
    let key_dir = key.dir.clone();
    let key_path = key_dir.join(KEY_FILE);
    let encrypted_key = std::fs::read(&key_path)?;
    let backup_path = plan.backup_path().to_owned();
    let backup = std::fs::read(&backup_path)?;
    std::fs::remove_file(&key_path)?;
    std::fs::create_dir(&key_path)?;
    let result = plan.record_receipt(
        &mut key,
        &json!({"network_id":state.genesis.network_id,"state":state,"now_ms":now_ms()}),
    );
    assert!(result.is_err());
    assert_eq!(
        key.network_id(),
        None,
        "failed persistence must not advance the in-memory identity"
    );
    assert_eq!(plan.state(), IdentityOperationState::Applied);
    let journal = std::fs::read(key_dir.join(PLAN_FILE))?;
    drop(plan);
    drop(key);
    std::fs::remove_dir(&key_path)?;
    privatefs::write_atomic(&key_path, &encrypted_key)?;
    let mut reopened = ClientKey::open(&key_dir, "client passphrase")?;
    assert!(reopened.network_id().is_none());
    assert_eq!(
        IdentityPlan::inspect(&reopened, "recovery password")?
            .context("Applied receipt must remain durable after the key write fails")?
            .state,
        IdentityOperationState::Applied
    );
    assert!(!IdentityPlan::discard_applied(
        &reopened,
        "recovery password"
    )?);
    assert_eq!(std::fs::read(key_dir.join(PLAN_FILE))?, journal);
    assert_eq!(std::fs::read(&backup_path)?, backup);
    let mut resumed = IdentityPlan::load(&reopened, "recovery password")?;
    resumed.record_receipt(
        &mut reopened,
        &json!({"network_id":state.genesis.network_id,"state":state,"now_ms":now_ms()}),
    )?;
    assert_eq!(
        reopened.network_id(),
        Some(state.genesis.network_id.as_str())
    );
    drop(resumed);
    assert!(IdentityPlan::discard_applied(
        &reopened,
        "recovery password"
    )?);
    assert_eq!(std::fs::read(&backup_path)?, backup);
    std::fs::remove_dir_all(dir)?;
    Ok(())
}
