use super::super::bot_settings::ChangeRequest;
use super::super::test_support::{
    Net, all_accepted, bot_with_message, memory_test_shown, net, say, second_bot, store,
};
use super::super::*;
use crate::providers::Provider;
use crate::scheduling::{JobOutcome, JobState, Shown, message_trigger};
use serde_json::json;
use vibeface_bridge::failure::{ExecFailure, FailureKind};

fn signed_memory(
    net: &Net,
    store: &Store,
    scope: &str,
    memory_id: &str,
    after: Option<&str>,
) -> Result<EventBody> {
    let base = store
        .memory_state(&net.network, scope, memory_id, None)?
        .current
        .map_or_else(MemoryBase::absent, |state| state.metadata.base());
    net.a.fixture_admin().attest(
        &net.network,
        scope,
        EventBody::MemoryControlled {
            memory_id: memory_id.into(),
            base,
            after: after.map(str::to_owned),
            attestation: None,
        },
    )
}

fn memory_command(
    network: &str,
    scope: &str,
    body: &EventBody,
) -> Result<bot_settings::MemoryControlCommand> {
    let EventBody::MemoryControlled {
        memory_id,
        base,
        after,
        attestation,
    } = body
    else {
        bail!("not memory control")
    };
    Ok(bot_settings::MemoryControlCommand {
        memory_id: memory_id.clone(),
        base: base.clone(),
        after: after.clone(),
        attestation: attestation.clone().context("signed memory")?,
        operation_id: crate::crypto::request_key(network, scope, body, "")?,
    })
}

#[test]
fn notes_refuse_secrets_and_sync() -> Result<()> {
    let net = net()?;
    let (mut origin, _) = bot_with_message(&net)?;
    let remember = |store: &mut Store, trigger: &str, call_ref: &str, text: &str| {
        store.change_bot(
            &net.a,
            &net.network,
            "c",
            trigger,
            call_ref,
            ChangeRequest::Remember {
                memory_id: None,
                text: text.into(),
            },
            Provider::Codex,
            10,
        )
    };
    for (index, refused) in [
        "my key is sk-proj-abcdefghijklmnopqrstuvwxyz123456".to_owned(),
        "Authorization: Bearer abcdefghijklmnopqrstuvwxyz".to_owned(),
        "-----BEGIN OPENSSH PRIVATE KEY-----".to_owned(),
        "x".repeat(MAX_MEMORY_CHARS + 1),
    ]
    .iter()
    .enumerate()
    {
        assert!(
            remember(&mut origin, "message:m1", &format!("n{index}"), refused).is_err(),
            "{refused}"
        );
    }
    let (kept, _) = remember(
        &mut origin,
        "message:m1",
        "k1",
        "Prefers short replies in Chinese",
    )?;
    let BotChange::Memory { memory_id, .. } = kept.change else {
        panic!("note")
    };
    let error = remember(&mut origin, "work:w1", "k2", "Something else").unwrap_err();
    assert!(error.to_string().contains(bot_settings::NOT_FROM_USER));
    let mut other = store(&net)?;
    all_accepted(&mut other, &net, &origin)?;
    assert_eq!(
        other.memories(&net.network, "c")?,
        origin.memories(&net.network, "c")?
    );
    origin.change_bot(
        &net.a,
        &net.network,
        "c",
        "message:m1",
        "k3",
        ChangeRequest::Forget { memory_id },
        Provider::Codex,
        11,
    )?;
    let mut other = store(&net)?;
    all_accepted(&mut other, &net, &origin)?;
    assert!(other.memories(&net.network, "c")?.is_empty());
    Ok(())
}

#[test]
fn staged_memory_commits_only_with_a_completed_turn() -> Result<()> {
    let net = net()?;
    let (mut store, _) = bot_with_message(&net)?;
    say(&mut store, &net, "m2", true)?;
    let trigger = message_trigger("m2");
    let claimed = store
        .claim_next_fixture(&net.a, &net.network, "c", "boot", "cfg")?
        .context("job")?;
    let attempt = claimed.job.state.attempt().context("attempt")?.id.clone();
    store.mark_started("c", &trigger, &attempt)?;
    store.save_scheduler_native_id("c", &trigger, &attempt, "thread", "token", false)?;
    let staged = store.stage_memory(
        &net.network,
        "c",
        &trigger,
        &attempt,
        "remember-1",
        ChangeRequest::Remember {
            memory_id: None,
            text: "prefers staged notes".into(),
        },
        4,
    )?;
    assert_eq!(staged["state"], "staged");
    assert_eq!(
        store.staged_memory_call("c", &trigger, &attempt, "remember-1")?,
        Some(staged)
    );
    assert!(
        store
            .staged_memory_call("c", &trigger, &attempt, "other")?
            .is_none()
    );
    assert!(
        store
            .staged_memory_call("c", &trigger, "other-attempt", "remember-1")
            .is_err()
    );
    assert!(store.memories(&net.network, "c")?.is_empty());
    store.record_manager_turn(
        &attempt,
        "system",
        "thread",
        Some("turn"),
        &Shown::default(),
    )?;
    store.finish_job(
        &net.a,
        &net.network,
        "c",
        &trigger,
        &attempt,
        JobOutcome::Completed,
        "done",
        5,
    )?;
    assert_eq!(store.memories(&net.network, "c")?.len(), 1);
    assert!(
        store
            .staged_memory_call("c", &trigger, &attempt, "remember-1")?
            .is_none()
    );
    assert!(
        store
            .bot_change(
                &net.network,
                &crate::store::bot_settings::change_id("c", &trigger, "remember-1")
            )?
            .is_some()
    );
    let record = store
        .memory_records(&net.network, "c")?
        .into_iter()
        .next()
        .context("memory record")?;
    assert_eq!(record.epoch, crate::config::DATA_EPOCH);
    assert!(!record.revision.is_empty());
    assert_eq!(record.origin, MemoryOrigin::Agent);
    assert!(!record.user_controlled);
    assert!(record.essential);

    say(&mut store, &net, "m3", true)?;
    let trigger = message_trigger("m3");
    let claimed = store
        .claim_next_fixture(&net.a, &net.network, "c", "boot-2", "cfg")?
        .context("job")?;
    let attempt = claimed.job.state.attempt().context("attempt")?.id.clone();
    store.mark_started("c", &trigger, &attempt)?;
    store.stage_memory(
        &net.network,
        "c",
        &trigger,
        &attempt,
        "remember-2",
        ChangeRequest::Remember {
            memory_id: None,
            text: "discard this note".into(),
        },
        6,
    )?;
    store.finish_job(
        &net.a,
        &net.network,
        "c",
        &trigger,
        &attempt,
        JobOutcome::Failed(ExecFailure::new(FailureKind::Rejected, "test")),
        "",
        7,
    )?;
    assert_eq!(store.memories(&net.network, "c")?.len(), 1);
    assert!(
        store
            .staged_memory_call("c", &trigger, &attempt, "remember-2")?
            .is_none()
    );
    Ok(())
}

#[test]
fn staged_memory_revision_conflict_preserves_the_newer_user_value() -> Result<()> {
    let net = net()?;
    let (mut store, _) = bot_with_message(&net)?;
    say(&mut store, &net, "m2", true)?;
    let trigger = message_trigger("m2");
    let claimed = store
        .claim_next_fixture(&net.a, &net.network, "c", "boot", "cfg")?
        .context("job")?;
    let attempt = claimed.job.state.attempt().context("attempt")?.id.clone();
    store.mark_started("c", &trigger, &attempt)?;
    let staged = store.stage_memory(
        &net.network,
        "c",
        &trigger,
        &attempt,
        "remember-conflict",
        ChangeRequest::Remember {
            memory_id: None,
            text: "agent candidate".into(),
        },
        4,
    )?;
    let memory_id = staged["memory_id"]
        .as_str()
        .context("memory id")?
        .to_owned();
    store.append_event(
        &net.a,
        &net.network,
        "c",
        signed_memory(&net, &store, "c", &memory_id, Some("user value"))?,
        vec![],
        5,
    )?;
    assert_eq!(
        store.stage_memory(
            &net.network,
            "c",
            &trigger,
            &attempt,
            "remember-conflict",
            ChangeRequest::Remember {
                memory_id: None,
                text: "agent candidate".into()
            },
            5,
        )?,
        staged,
        "a retry returns its original snapshot after a concurrent user edit"
    );
    assert!(
        store
            .stage_memory(
                &net.network,
                "c",
                &trigger,
                &attempt,
                "remember-conflict",
                ChangeRequest::Remember {
                    memory_id: None,
                    text: "another candidate".into()
                },
                5,
            )
            .is_err()
    );
    store.finish_job(
        &net.a,
        &net.network,
        "c",
        &trigger,
        &attempt,
        JobOutcome::Completed,
        "done",
        6,
    )?;
    let conflicts = memory_test_shown(&store)?.memory_conflicts;
    assert_eq!(conflicts.len(), 1);
    assert_eq!(conflicts[0].memory_id, memory_id);
    assert_eq!(conflicts[0].reason, "user_controlled");
    assert_eq!(conflicts[0].expected_revision, "");
    assert!(!conflicts[0].actual_revision.is_empty());
    let record = store
        .memory_records(&net.network, "c")?
        .into_iter()
        .find(|record| record.entry.memory_id == memory_id)
        .context("memory record")?;
    assert_eq!(record.entry.text, "user value");
    assert_eq!(record.origin, MemoryOrigin::User);
    assert!(record.user_controlled);
    Ok(())
}

#[test]
fn staged_memory_detects_a_revision_change_even_when_the_text_returns_to_its_base() -> Result<()> {
    let net = net()?;
    let (mut origin, _) = bot_with_message(&net)?;
    origin.change_bot(
        &net.a,
        &net.network,
        "c",
        "message:m1",
        "remember",
        ChangeRequest::Remember {
            memory_id: None,
            text: "base preference".into(),
        },
        Provider::Codex,
        4,
    )?;
    let record = origin.memory_records(&net.network, "c")?.remove(0);
    say(&mut origin, &net, "m2", true)?;
    let claimed = origin
        .claim_next_fixture(&net.a, &net.network, "c", "boot", "cfg")?
        .context("job")?;
    let trigger = &claimed.job.trigger;
    let attempt = &claimed.job.state.attempt().context("attempt")?.id;
    origin.mark_started("c", trigger, attempt)?;
    let request = ChangeRequest::Remember {
        memory_id: Some(record.entry.memory_id.clone()),
        text: "candidate preference".into(),
    };
    let staged = origin.stage_memory(
        &net.network,
        "c",
        trigger,
        attempt,
        "update",
        request.clone(),
        5,
    )?;
    for (index, before, after) in [
        (0, "base preference", "temporary preference"),
        (1, "temporary preference", "base preference"),
    ] {
        origin.append_event(
            &net.a,
            &net.network,
            "c",
            EventBody::BotChanged {
                change_id: format!("concurrent-{index}"),
                cause: Some(Cause {
                    conversation_id: "c".into(),
                    message_id: "m2".into(),
                }),
                change: BotChange::Memory {
                    memory_id: record.entry.memory_id.clone(),
                    before: Some(before.into()),
                    after: Some(after.into()),
                    base: origin
                        .memory_state(&net.network, "c", &record.entry.memory_id, None)?
                        .current
                        .context("base")?
                        .metadata
                        .base(),
                    origin: AutomaticMemoryOrigin::Agent,
                    essential: true,
                },
            },
            vec![],
            6 + index,
        )?;
    }
    assert_eq!(
        origin.stage_memory(&net.network, "c", trigger, attempt, "update", request, 8)?,
        staged
    );
    origin.finish_job(
        &net.a,
        &net.network,
        "c",
        trigger,
        attempt,
        JobOutcome::Completed,
        "done",
        9,
    )?;
    assert_eq!(origin.memories(&net.network, "c")?, vec![record.entry]);
    let conflicts = memory_test_shown(&origin)?.memory_conflicts;
    assert_eq!(conflicts.len(), 1);
    assert_eq!(conflicts[0].reason, "revision_changed");
    assert_eq!(conflicts[0].expected_revision, record.revision);
    assert_ne!(conflicts[0].actual_revision, record.revision);
    Ok(())
}

#[test]
fn staged_memory_is_discarded_for_failed_stopped_interrupted_and_unknown_turns() -> Result<()> {
    for (outcome, pending) in [
        (
            JobOutcome::Failed(ExecFailure::new(FailureKind::Rejected, "failed")),
            0,
        ),
        (JobOutcome::Cancelled("stopped".into()), 0),
        (
            JobOutcome::Interrupted {
                failure: ExecFailure::new(FailureKind::Transport, "interrupted"),
                process_unconfirmed: false,
            },
            0,
        ),
        (
            JobOutcome::Interrupted {
                failure: ExecFailure::new(FailureKind::Restarted, "unknown result"),
                process_unconfirmed: true,
            },
            1,
        ),
    ] {
        let net = net()?;
        let (mut origin, _) = bot_with_message(&net)?;
        say(&mut origin, &net, "m2", true)?;
        let claimed = origin
            .claim_next_fixture(&net.a, &net.network, "c", "boot", "cfg")?
            .context("job")?;
        let trigger = &claimed.job.trigger;
        let attempt = &claimed.job.state.attempt().context("attempt")?.id;
        origin.mark_started("c", trigger, attempt)?;
        origin.stage_memory(
            &net.network,
            "c",
            trigger,
            attempt,
            "remember",
            ChangeRequest::Remember {
                memory_id: None,
                text: "uncommitted preference".into(),
            },
            4,
        )?;
        origin.finish_job(
            &net.a,
            &net.network,
            "c",
            trigger,
            attempt,
            outcome,
            "partial",
            5,
        )?;
        assert!(origin.memories(&net.network, "c")?.is_empty());
        assert!(
            origin
                .staged_memory_overlay("c", trigger, attempt)?
                .is_empty()
        );
        assert_eq!(origin.pending_checks()?.len(), pending);
    }
    Ok(())
}

#[test]
fn staged_memory_reopens_replays_and_discards_a_restarted_unknown_attempt() -> Result<()> {
    let net = net()?;
    let (mut origin, _) = bot_with_message(&net)?;
    say(&mut origin, &net, "m2", true)?;
    let claimed = origin
        .claim_next_fixture(&net.a, &net.network, "c", "boot", "cfg")?
        .context("job")?;
    let trigger = &claimed.job.trigger;
    let attempt = &claimed.job.state.attempt().context("attempt")?.id;
    origin.mark_started("c", trigger, attempt)?;
    let request = ChangeRequest::Remember {
        memory_id: None,
        text: "persisted candidate".into(),
    };
    let staged = origin.stage_memory(
        &net.network,
        "c",
        trigger,
        attempt,
        "remember",
        request.clone(),
        4,
    )?;
    let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../.artifacts")
        .join(format!("memory-restart-{}", uuid::Uuid::new_v4()));
    crate::config::private_dir(&dir)?;
    let path = dir.join("node.sqlite3");
    origin
        .connection
        .execute("VACUUM INTO ?", [path.to_str().context("path")?])?;
    drop(origin);
    let mut reopened = Store::open(&path)?;
    reopened.recover_scheduler_jobs(&net.a, &net.network, "boot")?;
    assert_eq!(
        reopened.stage_memory(
            &net.network,
            "c",
            trigger,
            attempt,
            "remember",
            request.clone(),
            5
        )?,
        staged
    );
    reopened.finish_job(
        &net.a,
        &net.network,
        "c",
        trigger,
        attempt,
        JobOutcome::Completed,
        "done",
        6,
    )?;
    let initial = reopened.memory_records(&net.network, "c")?.remove(0);
    reopened.append_event(
        &net.a,
        &net.network,
        "c",
        signed_memory(
            &net,
            &reopened,
            "c",
            &initial.entry.memory_id,
            Some("newer committed user preference"),
        )?,
        vec![],
        7,
    )?;
    let version = reopened.version_vector(&net.network)?;
    let replay =
        reopened.stage_memory(&net.network, "c", trigger, attempt, "remember", request, 7)?;
    assert_eq!(replay["state"], "committed");
    assert_eq!(replay["record"]["change"]["after"], "persisted candidate");
    assert_eq!(reopened.version_vector(&net.network)?, version);
    let memory = reopened.memory_records(&net.network, "c")?.remove(0);
    assert_eq!(memory.entry.text, "newer committed user preference");
    say(&mut reopened, &net, "m3", true)?;
    let next = reopened
        .claim_next_fixture(&net.a, &net.network, "c", "boot", "cfg")?
        .context("next job")?;
    let next_attempt = &next.job.state.attempt().context("next attempt")?.id;
    reopened.mark_started("c", &next.job.trigger, next_attempt)?;
    reopened.stage_memory(
        &net.network,
        "c",
        &next.job.trigger,
        next_attempt,
        "discard",
        ChangeRequest::Remember {
            memory_id: None,
            text: "unknown candidate".into(),
        },
        8,
    )?;
    drop(reopened);
    let mut restarted = Store::open(&path)?;
    restarted.recover_scheduler_jobs(&net.a, &net.network, "new-boot")?;
    assert_eq!(
        restarted.memories(&net.network, "c")?,
        vec![memory.entry.clone()]
    );
    assert!(memory_test_shown(&restarted)?.staged_memories.is_empty());
    assert_eq!(restarted.pending_checks()?.len(), 1);
    let replayed = restarted.memory_records(&net.network, "c")?.remove(0);
    assert_eq!(replayed.revision, memory.revision);
    assert_eq!(replayed.source_event_hash, memory.source_event_hash);
    let mut replica = store(&net)?;
    all_accepted(&mut replica, &net, &restarted)?;
    assert_eq!(replica.memories(&net.network, "c")?, vec![memory.entry]);
    assert_eq!(
        replica.memory_records(&net.network, "c")?[0].revision,
        memory.revision
    );
    drop(restarted);
    std::fs::remove_dir_all(dir)?;
    Ok(())
}

#[test]
fn memory_control_preserves_tombstones_receipts_and_scope_isolation() -> Result<()> {
    let net = net()?;
    let (mut origin, _) = bot_with_message(&net)?;
    second_bot(&mut origin, &net, "Other")?;
    let create = signed_memory(&net, &origin, "c", "same-id", Some("first user value"))?;
    let command = memory_command(&net.network, "c", &create)?;
    let first = origin.control_memory(&net.a, &net.network, "c", command.clone(), 4)?;
    assert_eq!(first.state, MemoryControlStatus::Applied);
    let current = first.current.context("current")?;
    assert_eq!(current.text.as_deref(), Some("first user value"));
    assert_eq!(current.metadata.control_revision, 1);
    assert_eq!(current.metadata.origin, MemoryOrigin::User);
    assert!(current.metadata.user_controlled && current.metadata.essential);
    assert_eq!(
        origin
            .control_memory(&net.a, &net.network, "c", command.clone(), 5)?
            .state,
        MemoryControlStatus::AlreadyRecorded
    );
    assert_eq!(
        origin
            .memory_state(&net.network, "c", "same-id", Some(&command.operation_id))?
            .receipt,
        Some(MemoryReceipt::Recorded)
    );
    let other = signed_memory(&net, &origin, "d", "same-id", Some("independent scope"))?;
    origin.control_memory(
        &net.a,
        &net.network,
        "d",
        memory_command(&net.network, "d", &other)?,
        6,
    )?;
    assert_eq!(
        origin
            .memory_state(&net.network, "d", "same-id", Some(&command.operation_id))?
            .receipt,
        Some(MemoryReceipt::NotSeenHere)
    );
    let delete = signed_memory(&net, &origin, "c", "same-id", None)?;
    let deleted = origin.control_memory(
        &net.a,
        &net.network,
        "c",
        memory_command(&net.network, "c", &delete)?,
        7,
    )?;
    let tombstone = deleted.current.context("tombstone")?;
    assert_eq!(tombstone.text, None);
    assert_eq!(tombstone.metadata.control_revision, 2);
    assert!(tombstone.metadata.user_controlled);
    assert!(origin.memories(&net.network, "c")?.is_empty());
    assert_eq!(
        origin.memories(&net.network, "d")?[0].text,
        "independent scope"
    );
    assert_eq!(
        origin
            .control_memory(&net.a, &net.network, "c", command.clone(), 8)?
            .state,
        MemoryControlStatus::Superseded
    );
    assert_eq!(
        origin
            .memory_state(&net.network, "c", "same-id", Some(&command.operation_id))?
            .receipt,
        Some(MemoryReceipt::Superseded)
    );
    assert_eq!(
        origin
            .memory_state(&net.network, "c", "same-id", Some(&"0".repeat(64)))?
            .receipt,
        Some(MemoryReceipt::NotSeenHere)
    );
    assert_eq!(
        origin
            .memory_state(&net.network, "c", "same-id", None)?
            .receipt,
        None
    );
    assert!(
        origin
            .change_bot(
                &net.a,
                &net.network,
                "c",
                "message:m1",
                "resurrect",
                ChangeRequest::Remember {
                    memory_id: Some("same-id".into()),
                    text: "stale automatic resurrection".into(),
                },
                Provider::Codex,
                9
            )
            .unwrap_err()
            .to_string()
            .contains("user-controlled")
    );
    let restore = signed_memory(&net, &origin, "c", "same-id", Some("explicit restoration"))?;
    let restored = origin.control_memory(
        &net.a,
        &net.network,
        "c",
        memory_command(&net.network, "c", &restore)?,
        10,
    )?;
    assert_eq!(
        restored
            .current
            .context("restored")?
            .metadata
            .control_revision,
        3
    );
    let versions = origin.version_vector(&net.network)?;
    let mut forged = command;
    forged.operation_id = "0".repeat(64);
    assert!(
        origin
            .control_memory(&net.a, &net.network, "c", forged, 11)
            .is_err()
    );
    assert_eq!(origin.version_vector(&net.network)?, versions);
    assert!(
        origin
            .memory_sync_conflicts(&net.network, "c")?
            .items
            .is_empty()
    );
    Ok(())
}

#[test]
fn memory_user_operations_keep_stable_identity_across_carriers_and_rebuild() -> Result<()> {
    let net = net()?;
    let (mut a, _) = bot_with_message(&net)?;
    let mut b = store(&net)?;
    all_accepted(&mut b, &net, &a)?;
    let first_body = signed_memory(&net, &a, "c", "memory", Some("first"))?;
    let first_a = a.append_event(&net.a, &net.network, "c", first_body.clone(), vec![], 10)?;
    let first_b = b.append_event(&net.b, &net.network, "c", first_body.clone(), vec![], 20)?;
    let second_body = signed_memory(&net, &a, "c", "memory", Some("newer"))?;
    let second_a = a.append_event(&net.a, &net.network, "c", second_body.clone(), vec![], 30)?;
    b.ingest_events(
        &net.network,
        &a.events_from(&net.network, &net.a.device_id()?, 1, 500)?,
    )?;
    let second_b = b.append_event(&net.b, &net.network, "c", second_body.clone(), vec![], 40)?;
    let replay = b.append_event(&net.b, &net.network, "c", first_body.clone(), vec![], 50)?;
    let mut expected = None;
    let mut expected_conflicts = None;
    for reverse in [false, true] {
        let mut replica = store(&net)?;
        let mut events = a.events_from(&net.network, &net.a.device_id()?, 1, 500)?;
        let split = events.len();
        events.extend(b.events_from(&net.network, &net.b.device_id()?, 1, 500)?);
        if reverse {
            events.rotate_left(split);
        }
        for event in &events {
            let statuses = replica
                .ingest_events(&net.network, std::slice::from_ref(event))?
                .statuses;
            assert!(
                statuses
                    .iter()
                    .all(|status| matches!(status, IngestStatus::Accepted | IngestStatus::Pending)),
                "{statuses:?}"
            );
        }
        let state = replica
            .memory_state(&net.network, "c", "memory", None)?
            .current
            .context("current")?;
        assert_eq!(state.text.as_deref(), Some("newer"));
        assert_eq!(state.metadata.control_revision, 2);
        assert_eq!(
            state.metadata.source_event_hash,
            second_a.hash.clone().min(second_b.hash.clone())
        );
        assert_eq!(
            state.metadata.revision,
            crate::crypto::request_key(&net.network, "c", &second_body, "")?
        );
        let changed_at: i64 = replica.connection.query_row("SELECT changed_at_ms FROM memories WHERE network_id=? AND conversation_id='c' AND memory_id='memory'", [&net.network], |row| row.get(0))?;
        assert_eq!(
            changed_at,
            if second_a.hash < second_b.hash {
                30
            } else {
                40
            }
        );
        let conflicts = replica.memory_sync_conflicts(&net.network, "c")?;
        assert!(
            conflicts.items.is_empty(),
            "duplicate old carriers are one logical ancestor"
        );
        if let Some(expected) = &expected {
            assert_eq!(&state, expected);
        }
        if let Some(expected) = &expected_conflicts {
            assert_eq!(&conflicts, expected);
        }
        expected = Some(state.clone());
        expected_conflicts = Some(conflicts.clone());
        replica.connection.execute(
            "UPDATE events SET effective=0 WHERE network_id=?",
            [&net.network],
        )?;
        assert!(log::recompute(
            &replica.connection,
            &membership_from(&replica.connection, &net.network)?
        )?);
        assert_eq!(
            replica
                .memory_state(&net.network, "c", "memory", None)?
                .current,
            Some(state)
        );
        assert_eq!(replica.memory_sync_conflicts(&net.network, "c")?, conflicts);
    }
    let context_entries = |store: &Store| -> Result<Vec<crate::agent_context::MemoryContextEntry>> {
        Ok(store
            .memory_records(&net.network, "c")?
            .into_iter()
            .map(|record| crate::agent_context::MemoryContextEntry {
                memory_id: record.entry.memory_id,
                text: record.entry.text,
                epoch: record.epoch,
                revision: record.revision,
            })
            .collect())
    };
    let delivered = crate::agent_context::compile_memory(&context_entries(&a)?, None)?;
    a.ingest_events(
        &net.network,
        &b.events_from(&net.network, &net.b.device_id()?, 1, 500)?,
    )?;
    let duplicate_delivery =
        crate::agent_context::compile_memory(&context_entries(&a)?, Some(&delivered.shown))?;
    assert!(
        matches!(duplicate_delivery.context, crate::agent_context::MemoryContext::Delta { upserts, deleted, omitted: 0, .. }
        if upserts.is_empty() && deleted.is_empty())
    );
    assert_eq!(duplicate_delivery.shown, delivered.shown);
    let mut old_source_base = a
        .memory_state(&net.network, "c", "memory", None)?
        .current
        .context("base")?
        .metadata
        .base();
    old_source_base
        .version
        .as_mut()
        .context("version")?
        .source_event_hash = second_a.hash.max(second_b.hash);
    let delete = net.a.fixture_admin().attest(
        &net.network,
        "c",
        EventBody::MemoryControlled {
            memory_id: "memory".into(),
            base: old_source_base,
            after: None,
            attestation: None,
        },
    )?;
    assert_eq!(
        a.control_memory(
            &net.a,
            &net.network,
            "c",
            memory_command(&net.network, "c", &delete)?,
            60
        )?
        .state,
        MemoryControlStatus::Applied
    );
    assert!(
        a.memory_state(&net.network, "c", "memory", None)?
            .current
            .context("deleted")?
            .text
            .is_none()
    );
    assert_eq!(
        a.control_memory(
            &net.a,
            &net.network,
            "c",
            memory_command(&net.network, "c", &first_body)?,
            70
        )?
        .state,
        MemoryControlStatus::Superseded
    );
    assert_ne!(first_a.hash, first_b.hash);
    assert_ne!(replay.hash, first_a.hash);
    Ok(())
}

#[test]
fn memory_user_tombstones_survive_stale_automatic_branches_in_every_order() -> Result<()> {
    let net = net()?;
    let (mut a, _) = bot_with_message(&net)?;
    a.change_bot(
        &net.a,
        &net.network,
        "c",
        "message:m1",
        "seed",
        ChangeRequest::Remember {
            memory_id: None,
            text: "automatic base".into(),
        },
        Provider::Codex,
        4,
    )?;
    let seed = a.memory_records(&net.network, "c")?.remove(0);
    let mut b = store(&net)?;
    all_accepted(&mut b, &net, &a)?;
    b.change_bot(
        &net.b,
        &net.network,
        "c",
        "message:m1",
        "stale",
        ChangeRequest::Remember {
            memory_id: Some(seed.entry.memory_id.clone()),
            text: "stale automatic branch".into(),
        },
        Provider::Codex,
        50,
    )?;
    let user_body = signed_memory(&net, &a, "c", &seed.entry.memory_id, Some("user value"))?;
    a.append_event(&net.a, &net.network, "c", user_body, vec![], 6)?;
    let delete_body = signed_memory(&net, &a, "c", &seed.entry.memory_id, None)?;
    a.append_event(&net.a, &net.network, "c", delete_body, vec![], 7)?;
    let mut expected = None;
    for reverse in [false, true] {
        let mut replica = store(&net)?;
        let mut events = a.events_from(&net.network, &net.a.device_id()?, 1, 500)?;
        let split = events.len();
        events.extend(b.events_from(&net.network, &net.b.device_id()?, 1, 500)?);
        if reverse {
            events.rotate_left(split);
        }
        replica.ingest_events(&net.network, &events)?;
        let state = replica
            .memory_state(&net.network, "c", &seed.entry.memory_id, None)?
            .current
            .context("tombstone")?;
        assert_eq!(state.text, None);
        assert_eq!(state.metadata.control_revision, 2);
        assert!(state.metadata.user_controlled);
        let conflicts = replica.memory_sync_conflicts(&net.network, "c")?;
        assert_eq!(conflicts.items.len(), 1);
        assert_eq!(
            conflicts.items[0].reason,
            MemorySyncConflictReason::UserProtected
        );
        assert_eq!(conflicts.items[0].winning_revision, state.metadata.revision);
        if let Some(expected) = &expected {
            assert_eq!(&(state.clone(), conflicts.clone()), expected);
        }
        expected = Some((state, conflicts));
    }
    Ok(())
}

#[test]
fn memory_base_authentication_rejects_forged_versions_and_waits_for_real_parents() -> Result<()> {
    let net = net()?;
    let (mut origin, _) = bot_with_message(&net)?;
    origin.change_bot(
        &net.a,
        &net.network,
        "c",
        "message:m1",
        "seed",
        ChangeRequest::Remember {
            memory_id: None,
            text: "base".into(),
        },
        Provider::Codex,
        4,
    )?;
    let record = origin.memory_records(&net.network, "c")?.remove(0);
    let base = record.metadata().base();
    for (scope, memory_id, mut claimed, direct) in [
        ("c", record.entry.memory_id.as_str(), base.clone(), false),
        ("c", "another-memory", base.clone(), true),
        ("d", record.entry.memory_id.as_str(), base.clone(), true),
        ("c", record.entry.memory_id.as_str(), base.clone(), true),
    ] {
        if scope == "c" && memory_id == record.entry.memory_id && direct {
            claimed
                .version
                .as_mut()
                .context("version")?
                .control_revision = 100;
        }
        let body = net.a.fixture_admin().attest(
            &net.network,
            scope,
            EventBody::MemoryControlled {
                memory_id: memory_id.into(),
                base: claimed,
                after: Some("forged".into()),
                attestation: None,
            },
        )?;
        let envelope = net.b.sign_event(Event {
            version: WIRE_VERSION,
            network_id: net.network.clone(),
            author: net.b.device_id()?,
            seq: 1,
            prev: None,
            deps: if direct {
                vec![record.source_event_hash.clone()]
            } else {
                vec![]
            },
            scope_id: scope.into(),
            created_at_ms: 5,
            body,
            blobs: vec![],
        })?;
        assert!(matches!(
            origin
                .ingest_events(&net.network, &[envelope])?
                .statuses
                .as_slice(),
            [IngestStatus::Rejected(_)]
        ));
    }
    let body = signed_memory(
        &net,
        &origin,
        "c",
        &record.entry.memory_id,
        Some("real dependent user value"),
    )?;
    let dependent = net.b.sign_event(Event {
        version: WIRE_VERSION,
        network_id: net.network.clone(),
        author: net.b.device_id()?,
        seq: 1,
        prev: None,
        deps: vec![record.source_event_hash.clone()],
        scope_id: "c".into(),
        created_at_ms: 6,
        body: body.clone(),
        blobs: vec![],
    })?;
    let mut replica = store(&net)?;
    assert_eq!(
        replica.ingest_events(&net.network, &[dependent])?.statuses,
        vec![IngestStatus::Pending]
    );
    all_accepted(&mut replica, &net, &origin)?;
    let result = replica.memory_state(
        &net.network,
        "c",
        &record.entry.memory_id,
        Some(&crate::crypto::request_key(&net.network, "c", &body, "")?),
    )?;
    assert_eq!(result.receipt, Some(MemoryReceipt::Recorded));
    assert_eq!(
        result.current.context("dependent")?.text.as_deref(),
        Some("real dependent user value")
    );
    let body = net.a.attest(
        &net.network,
        "c",
        EventBody::MemoryControlled {
            memory_id: "node-forgery".into(),
            base: MemoryBase::absent(),
            after: Some("node cannot impersonate user".into()),
            attestation: None,
        },
    )?;
    assert!(
        origin
            .append_event(&net.a, &net.network, "c", body, vec![], 7)
            .is_err()
    );
    assert!(
        origin
            .append_event(
                &net.a,
                &net.network,
                "c",
                EventBody::MemoryControlled {
                    memory_id: "unsigned".into(),
                    base: MemoryBase::absent(),
                    after: Some("unsigned value".into()),
                    attestation: None,
                },
                vec![],
                8
            )
            .is_err()
    );
    assert!(
        serde_json::from_value::<MemoryBase>(json!({"epoch":crate::config::DATA_EPOCH})).is_err()
    );
    let mut overflowing = base;
    overflowing
        .version
        .as_mut()
        .context("version")?
        .control_revision = u32::MAX;
    assert!(overflowing.next_control_revision().is_err());
    for value in [
        json!({"kind":"memory","memory_id":"m","before":null,"after":"value","base":{"epoch":crate::config::DATA_EPOCH,"version":null},"origin":"user","essential":true}),
        json!({"kind":"memory","memory_id":"m","before":null,"after":"value","base":{"epoch":crate::config::DATA_EPOCH,"version":null},"origin":"agent","user_controlled":true,"essential":true}),
    ] {
        assert!(serde_json::from_value::<BotChange>(value).is_err());
    }
    Ok(())
}

#[test]
fn memory_sync_conflicts_follow_logical_bases_for_automatic_and_user_branches() -> Result<()> {
    let net = net()?;
    let (mut a, _) = bot_with_message(&net)?;
    a.change_bot(
        &net.a,
        &net.network,
        "c",
        "message:m1",
        "seed",
        ChangeRequest::Remember {
            memory_id: None,
            text: "seed".into(),
        },
        Provider::Codex,
        4,
    )?;
    let seed = a.memory_records(&net.network, "c")?.remove(0);
    a.change_bot(
        &net.a,
        &net.network,
        "c",
        "message:m1",
        "linear",
        ChangeRequest::Remember {
            memory_id: Some(seed.entry.memory_id.clone()),
            text: "linear update".into(),
        },
        Provider::Codex,
        5,
    )?;
    assert!(a.memory_sync_conflicts(&net.network, "c")?.items.is_empty());
    let mut b = store(&net)?;
    all_accepted(&mut b, &net, &a)?;
    for (store, author, call, text) in [
        (&mut a, &net.a, "left", "left branch"),
        (&mut b, &net.b, "right", "right branch"),
    ] {
        store.change_bot(
            author,
            &net.network,
            "c",
            "message:m1",
            call,
            ChangeRequest::Remember {
                memory_id: Some(seed.entry.memory_id.clone()),
                text: text.into(),
            },
            Provider::Codex,
            6,
        )?;
    }
    a.ingest_events(
        &net.network,
        &b.events_from(&net.network, &net.b.device_id()?, 1, 500)?,
    )?;
    let automatic = a.memory_sync_conflicts(&net.network, "c")?;
    assert_eq!(automatic.items.len(), 1);
    assert_eq!(
        automatic.items[0].reason,
        MemorySyncConflictReason::ConcurrentAutomatic
    );
    let takeover = signed_memory(
        &net,
        &a,
        "c",
        &seed.entry.memory_id,
        a.memory_state(&net.network, "c", &seed.entry.memory_id, None)?
            .current
            .context("current")?
            .text
            .as_deref(),
    )?;
    a.append_event(&net.a, &net.network, "c", takeover, vec![], 7)?;
    let protected = a.memory_sync_conflicts(&net.network, "c")?;
    assert_eq!(protected.items.len(), 1);
    assert_eq!(
        protected.items[0].losing_revision,
        automatic.items[0].losing_revision
    );
    assert_eq!(
        protected.items[0].reason,
        MemorySyncConflictReason::UserProtected
    );
    let versions = a.version_vector(&net.network)?;
    let stale = signed_memory(
        &net,
        &b,
        "c",
        &seed.entry.memory_id,
        Some("stale local edit"),
    )?;
    assert_eq!(
        a.control_memory(
            &net.a,
            &net.network,
            "c",
            memory_command(&net.network, "c", &stale)?,
            8
        )?
        .state,
        MemoryControlStatus::Conflict
    );
    assert_eq!(a.version_vector(&net.network)?, versions);
    Ok(())
}

#[test]
fn memory_sync_conflicts_scan_all_history_before_bounding_private_metadata() -> Result<()> {
    let net = net()?;
    let (mut origin, _) = bot_with_message(&net)?;
    for index in 0..55 {
        let body = net.a.fixture_admin().attest(
            &net.network,
            "c",
            EventBody::MemoryControlled {
                memory_id: "memory".into(),
                base: MemoryBase::absent(),
                after: Some(format!("private candidate {index}")),
                attestation: None,
            },
        )?;
        origin.append_event(&net.a, &net.network, "c", body, vec![], 10 + index)?;
    }
    let page = origin.memory_sync_conflicts(&net.network, "c")?;
    assert_eq!(page.items.len(), 50);
    assert!(page.has_more);
    assert!(
        page.items
            .windows(2)
            .all(|pair| pair[0].losing_revision < pair[1].losing_revision)
    );
    assert!(
        page.items
            .iter()
            .all(|item| item.reason == MemorySyncConflictReason::ConcurrentUser)
    );
    assert!(!serde_json::to_string(&page)?.contains("private candidate"));
    let mut replica = store(&net)?;
    let events = origin.events_from(&net.network, &net.a.device_id()?, 1, 500)?;
    for chunk in events.chunks(3) {
        replica.ingest_events(&net.network, chunk)?;
    }
    assert_eq!(replica.memory_sync_conflicts(&net.network, "c")?, page);
    assert_eq!(
        replica.memory_state(&net.network, "c", "memory", None)?,
        origin.memory_state(&net.network, "c", "memory", None)?
    );
    Ok(())
}

#[test]
fn memory_control_applies_live_limits_and_rejects_credentials_before_writing() -> Result<()> {
    let net = net()?;
    let (mut origin, _) = bot_with_message(&net)?;
    for index in 0..MAX_MEMORIES {
        let id = format!("memory-{index}");
        let body = signed_memory(&net, &origin, "c", &id, Some("user note"))?;
        origin.control_memory(
            &net.a,
            &net.network,
            "c",
            memory_command(&net.network, "c", &body)?,
            10 + index as i64,
        )?;
    }
    let extra = signed_memory(&net, &origin, "c", "extra", Some("another user note"))?;
    let version = origin.version_vector(&net.network)?;
    assert!(
        origin
            .control_memory(
                &net.a,
                &net.network,
                "c",
                memory_command(&net.network, "c", &extra)?,
                70
            )
            .is_err()
    );
    assert_eq!(origin.version_vector(&net.network)?, version);
    let delete = signed_memory(&net, &origin, "c", "memory-0", None)?;
    origin.control_memory(
        &net.a,
        &net.network,
        "c",
        memory_command(&net.network, "c", &delete)?,
        71,
    )?;
    assert_eq!(
        origin
            .control_memory(
                &net.a,
                &net.network,
                "c",
                memory_command(&net.network, "c", &extra)?,
                72
            )?
            .state,
        MemoryControlStatus::Applied
    );
    let restore = signed_memory(&net, &origin, "c", "memory-0", Some("restore"))?;
    assert!(
        origin
            .control_memory(
                &net.a,
                &net.network,
                "c",
                memory_command(&net.network, "c", &restore)?,
                73
            )
            .is_err()
    );
    assert_eq!(origin.memories(&net.network, "c")?.len(), MAX_MEMORIES);
    assert_eq!(
        origin
            .memory_state(&net.network, "c", "memory-0", None)?
            .current
            .context("tombstone")?
            .text,
        None
    );
    for text in [
        "x".repeat(MAX_MEMORY_CHARS + 1),
        "password=fixture-value".into(),
    ] {
        let invalid = signed_memory(&net, &origin, "c", "memory-1", Some(&text))?;
        let version = origin.version_vector(&net.network)?;
        assert!(
            origin
                .control_memory(
                    &net.a,
                    &net.network,
                    "c",
                    memory_command(&net.network, "c", &invalid)?,
                    74
                )
                .is_err()
        );
        assert_eq!(origin.version_vector(&net.network)?, version);
    }
    Ok(())
}

#[test]
fn staged_memory_capacity_is_rechecked_at_completion() -> Result<()> {
    for (live_count, staged_count, restore) in [(49, 1, false), (48, 2, false), (49, 1, true)] {
        let net = net()?;
        let (mut origin, _) = bot_with_message(&net)?;
        let tombstone = if restore {
            origin.change_bot(
                &net.a,
                &net.network,
                "c",
                "message:m1",
                "deleted-seed",
                ChangeRequest::Remember {
                    memory_id: None,
                    text: "old automatic note".into(),
                },
                Provider::Codex,
                4,
            )?;
            let memory_id = origin.memories(&net.network, "c")?.remove(0).memory_id;
            origin.change_bot(
                &net.a,
                &net.network,
                "c",
                "message:m1",
                "delete-seed",
                ChangeRequest::Forget {
                    memory_id: memory_id.clone(),
                },
                Provider::Codex,
                5,
            )?;
            origin
                .memory_state(&net.network, "c", &memory_id, None)?
                .current
        } else {
            None
        };
        for index in 0..live_count {
            let body = signed_memory(
                &net,
                &origin,
                "c",
                &format!("user-{index}"),
                Some("saved user note"),
            )?;
            origin.control_memory(
                &net.a,
                &net.network,
                "c",
                memory_command(&net.network, "c", &body)?,
                10 + index,
            )?;
        }
        say(&mut origin, &net, "m2", true)?;
        let trigger = message_trigger("m2");
        let claimed = origin
            .claim_next_fixture(&net.a, &net.network, "c", "boot", "cfg")?
            .context("job")?;
        let attempt = claimed.job.state.attempt().context("attempt")?.id.clone();
        origin.mark_started("c", &trigger, &attempt)?;
        let mut staged_ids = Vec::new();
        for index in 0..staged_count {
            let staged = origin.stage_memory(
                &net.network,
                "c",
                &trigger,
                &attempt,
                &format!("stage-{index}"),
                ChangeRequest::Remember {
                    memory_id: tombstone.as_ref().map(|state| state.memory_id.clone()),
                    text: format!("staged candidate {index}"),
                },
                70,
            )?;
            staged_ids.push(
                staged["memory_id"]
                    .as_str()
                    .context("staged memory ID")?
                    .to_owned(),
            );
        }
        let incoming = signed_memory(
            &net,
            &origin,
            "c",
            "incoming-user",
            Some("new signed user memory"),
        )?;
        origin.control_memory(
            &net.a,
            &net.network,
            "c",
            memory_command(&net.network, "c", &incoming)?,
            71,
        )?;
        origin.finish_job(
            &net.a,
            &net.network,
            "c",
            &trigger,
            &attempt,
            JobOutcome::Completed,
            "completed reply",
            72,
        )?;
        assert_eq!(origin.memories(&net.network, "c")?.len(), MAX_MEMORIES);
        assert!(matches!(
            origin
                .scheduler_job("c", &trigger)?
                .context("completed job")?
                .state,
            JobState::Completed { .. }
        ));
        let reply = origin
            .messages(&net.network, "c")?
            .into_iter()
            .find(|message| message.in_reply_to.as_deref() == Some("m2"))
            .context("reply")?;
        assert_eq!(reply.text, "completed reply");
        assert_eq!(reply.status, Some(ReplyStatus::Completed));
        let conflicts = memory_test_shown(&origin)?.memory_conflicts;
        assert_eq!(conflicts.len(), 1);
        assert_eq!(conflicts[0].reason, "capacity_exceeded");
        assert_eq!(
            conflicts[0].memory_id,
            *staged_ids.last().context("rejected memory")?
        );
        for (index, memory_id) in staged_ids.iter().enumerate() {
            let committed = index < (MAX_MEMORIES - live_count as usize - 1);
            assert_eq!(
                origin
                    .bot_change(
                        &net.network,
                        &bot_settings::change_id("c", &trigger, &format!("stage-{index}"))
                    )?
                    .is_some(),
                committed
            );
            if committed {
                assert_eq!(
                    origin
                        .memory_state(&net.network, "c", memory_id, None)?
                        .current
                        .context("committed memory")?
                        .text,
                    Some(format!("staged candidate {index}"))
                );
            }
        }
        if let Some(tombstone) = tombstone {
            assert_eq!(
                origin
                    .memory_state(&net.network, "c", &tombstone.memory_id, None)?
                    .current
                    .as_ref(),
                Some(&tombstone)
            );
        }
    }
    Ok(())
}
