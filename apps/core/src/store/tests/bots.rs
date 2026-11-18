use super::super::bot_settings::ChangeRequest;
use super::super::test_support::{
    Net, all_accepted, bot, bot_conversation, bot_reply, bot_with_message, conversation, listed,
    net, rename, save_schedule, save_skill, second_bot, signed_user, store, sync, user,
    workspace_command_fixture, workspace_fixture, workspace_input_fixture, workspace_task_command,
};
use super::super::*;
use crate::providers::Provider;
use crate::scheduling::{JobState, TriggerKind, bot_message_trigger, message_trigger};
use serde_json::json;

fn bot_projection(store: &Store, net: &Net) -> Result<Value> {
    let changes: Vec<_> = store
        .bot_changes(&net.network, "c")?
        .into_iter()
        .map(|change| (change.change_id, change.event_hash, change.change))
        .collect();
    Ok(serde_json::json!({
        "bots": store.bots(&net.network)?,
        "skills": store.skills(&net.network)?,
        "schedules": store.schedules(&net.network, "c")?,
        "changes": changes,
    }))
}

#[test]
fn a_signed_main_input_authorizes_bot_lifecycle_locally_and_on_replicas() -> Result<()> {
    let (net, mut origin) = workspace_fixture()?;
    let input = workspace_input_fixture(
        &net,
        &mut origin,
        "main-profile",
        None,
        crate::workspace::InputIntent::Continue,
        3,
    )?;
    let trigger = message_trigger(&input.input.message_id);
    origin.change_bot(
        &net.a,
        &net.network,
        "c",
        &trigger,
        "rename-main",
        rename("Nova"),
        Provider::Codex,
        4,
    )?;
    let (created, new) = origin.create_bot_in_turn(
        &net.a,
        &net.network,
        "c",
        &trigger,
        "create-main",
        ("Typed child", "follow the signed Main input", None),
        5,
    )?;
    assert!(new);
    let (_, deleted) = origin.delete_bot_in_turn(
        &net.a,
        &net.network,
        "c",
        &trigger,
        &created.conversation_id,
        6,
    )?;
    assert!(deleted.is_some());
    assert_eq!(origin.bots(&net.network)?[0].name, "Nova");
    assert_eq!(origin.bot_changes(&net.network, "c")?.len(), 1);
    let mut replica = store(&net)?;
    let events = origin.events_from(&net.network, &net.a.device_id()?, 1, 100)?;
    assert_eq!(
        replica.ingest_events(&net.network, &events)?.statuses,
        vec![IngestStatus::Accepted; events.len()]
    );
    assert_eq!(replica.bots(&net.network)?[0].name, "Nova");
    assert_eq!(replica.bot_changes(&net.network, "c")?.len(), 1);
    assert_eq!(
        replica
            .conversation(&net.network, &created.conversation_id)?
            .and_then(|conversation| conversation.owner),
        Some(net.a.device_id()?)
    );
    assert!(replica.bot_deleted(&net.network, &created.conversation_id)?);
    Ok(())
}

#[test]
fn typed_bot_changes_reject_task_answers_forged_causes_and_indirect_sources() -> Result<()> {
    use crate::workspace::InputIntent;
    let (net, mut origin) = workspace_fixture()?;
    workspace_command_fixture(
        &net,
        &mut origin,
        "create-task",
        workspace_task_command("task-source", None),
        3,
    )?;
    let main = workspace_input_fixture(
        &net,
        &mut origin,
        "main-source",
        None,
        InputIntent::Continue,
        4,
    )?;
    let task = workspace_input_fixture(
        &net,
        &mut origin,
        "task-input",
        Some("task-source"),
        InputIntent::Continue,
        5,
    )?;
    let answer = origin.append_event(
        &net.a,
        &net.network,
        "c",
        net.a.fixture_admin().attest(
            &net.network,
            "c",
            EventBody::MessagePosted {
                message_id: "a-q-source".into(),
                role: Role::User,
                text: "answer only".into(),
                attachments: vec![],
                report_results: Some(true),
                in_reply_to: None,
                status: None,
                answers: Some("q-source".into()),
                attestation: None,
                failure: None,
            },
        )?,
        vec![],
        6,
    )?;
    let indirect = origin.append_event(
        &net.a,
        &net.network,
        "c",
        bot_reply("after-main", "main-source"),
        vec![],
        7,
    )?;
    second_bot(&mut origin, &net, "Other")?;
    let mut target = store(&net)?;
    let events = origin.events_from(&net.network, &net.a.device_id()?, 1, 100)?;
    assert_eq!(
        target.ingest_events(&net.network, &events)?.statuses,
        vec![IngestStatus::Accepted; events.len()]
    );
    let change = |conversation: &str, message: &str, deps: Vec<String>| {
        net.b.sign_event(Event {
            version: WIRE_VERSION,
            network_id: net.network.clone(),
            author: net.b.device_id()?,
            seq: 1,
            prev: None,
            deps,
            scope_id: "c".into(),
            created_at_ms: 8,
            body: EventBody::BotChanged {
                change_id: "typed-change".into(),
                cause: Some(Cause {
                    conversation_id: conversation.into(),
                    message_id: message.into(),
                }),
                change: BotChange::Profile {
                    name: Some(Edit {
                        before: "Workspace Bot".into(),
                        after: "Nova".into(),
                    }),
                    instructions: None,
                    model: None,
                    skills: None,
                    avatar: None,
                    reasoning_effort: None,
                },
            },
            blobs: vec![],
        })
    };
    for (conversation, message, deps) in [
        ("c", "task-input", vec![task.source_event]),
        ("c", "a-q-source", vec![answer.hash]),
        ("c", "wrong-input", vec![main.source_event.clone()]),
        ("other", "main-source", vec![main.source_event.clone()]),
        ("c", "after-main", vec![indirect.hash.clone()]),
        ("c", "main-source", vec![indirect.hash]),
        ("c", "main-source", vec![]),
    ] {
        assert!(
            matches!(&target.ingest_events(&net.network, &[change(conversation, message, deps)?])?.statuses[0],
                IngestStatus::Rejected(error) if error.contains("user's own signed message")),
            "{conversation}/{message}"
        );
        assert_eq!(
            target
                .bot_for_conversation(&net.network, "c")?
                .context("source Bot")?
                .name,
            "Workspace Bot"
        );
        assert!(target.bot_changes(&net.network, "c")?.is_empty());
    }
    let hijack = net.b.sign_event(Event {
        version: WIRE_VERSION,
        network_id: net.network.clone(),
        author: net.b.device_id()?,
        seq: 1,
        prev: None,
        deps: vec![main.source_event.clone()],
        scope_id: "d".into(),
        created_at_ms: 8,
        body: EventBody::ConversationOwner {
            scheduler_node: net.b.device_id()?,
            epoch: 1,
            attestation: None,
            cause: Some(Cause {
                conversation_id: "c".into(),
                message_id: "main-source".into(),
            }),
        },
        blobs: vec![],
    })?;
    assert!(
        matches!(&target.ingest_events(&net.network, &[hijack])?.statuses[0],
            IngestStatus::Rejected(error) if error.contains("a created bot's first owner follows its creation"))
    );
    assert_eq!(
        target
            .conversation(&net.network, "d")?
            .and_then(|conversation| conversation.owner),
        Some(net.a.device_id()?)
    );
    assert_eq!(
        target
            .ingest_events(
                &net.network,
                &[change("c", "main-source", vec![main.source_event])?],
            )?
            .statuses,
        vec![IngestStatus::Accepted]
    );
    assert_eq!(
        target
            .bot_for_conversation(&net.network, "c")?
            .context("source Bot")?
            .name,
        "Nova"
    );
    assert_eq!(target.bot_changes(&net.network, "c")?.len(), 1);
    for trigger in ["work:w1", "message:after-main"] {
        let error = origin
            .change_bot(
                &net.a,
                &net.network,
                "c",
                trigger,
                "r1",
                rename("Nova"),
                Provider::Codex,
                9,
            )
            .unwrap_err();
        assert!(error.to_string().contains(bot_settings::NOT_FROM_USER));
    }
    Ok(())
}

#[test]
fn a_bot_setting_two_devices_changed_at_once_names_the_change_that_lost() -> Result<()> {
    use bot_settings::{Overwritten, Setting};
    let net = net()?;
    let (mut a_side, _) = bot_with_message(&net)?;
    let mut b_side = store(&net)?;
    sync(&a_side, &mut b_side, &net)?;
    b_side.append_event(&net.b, &net.network, "c", user(&net, "m2", "y"), vec![], 4)?;
    let rename_on = |store: &mut Store, identity, trigger, call_ref, name, at| {
        store.change_bot(
            identity,
            &net.network,
            "c",
            trigger,
            call_ref,
            rename(name),
            Provider::Codex,
            at,
        )
    };
    let (alpha, _) = rename_on(&mut a_side, &net.a, "message:m1", "r1", "Alpha", 5)?;
    let (beta, _) = rename_on(&mut b_side, &net.b, "message:m2", "r1", "Beta", 5)?;
    sync(&a_side, &mut b_side, &net)?;
    sync(&b_side, &mut a_side, &net)?;

    let stands = a_side.bots(&net.network)?[0].name.clone();
    let (lost, won, winner) = if stands == "Beta" {
        (alpha, "Beta", &net.b)
    } else {
        (beta, "Alpha", &net.a)
    };
    let expected = vec![Overwritten {
        change_id: lost.change_id.clone(),
        setting: Setting::Name,
        by: winner.device_id()?,
        value: json!(won),
    }];
    for side in [&a_side, &b_side] {
        assert_eq!(
            side.bots(&net.network)?[0].name,
            won,
            "the larger rank still wins"
        );
        assert_eq!(side.overwritten(&net.network, "c")?, expected);
    }

    let replayed = b_side.ingest_events(
        &net.network,
        &a_side.events_from(&net.network, &net.a.device_id()?, 1, 500)?,
    )?;
    assert!(
        replayed
            .statuses
            .iter()
            .all(|status| *status == IngestStatus::Duplicate)
    );
    assert_eq!(
        b_side.overwritten(&net.network, "c")?,
        expected,
        "a replay adds nothing"
    );
    a_side.connection.execute(
        "UPDATE events SET effective=0 WHERE network_id=?",
        [&net.network],
    )?;
    reduce::clear(&a_side.connection, &net.network)?;
    log::recompute(
        &a_side.connection,
        &membership_from(&a_side.connection, &net.network)?,
    )?;
    assert_eq!(
        a_side.overwritten(&net.network, "c")?,
        expected,
        "a rebuild adds nothing"
    );
    let mut late = store(&net)?;
    sync(&b_side, &mut late, &net)?;
    assert_eq!(
        late.overwritten(&net.network, "c")?,
        expected,
        "a new replica agrees"
    );

    let (loser_side, loser, trigger, mine) = if won == "Beta" {
        (&mut a_side, &net.a, "message:m1", "Alpha")
    } else {
        (&mut b_side, &net.b, "message:m2", "Beta")
    };
    rename_on(loser_side, loser, trigger, "r2", mine, 9)?;
    sync(&a_side, &mut b_side, &net)?;
    sync(&b_side, &mut a_side, &net)?;
    for side in [&a_side, &b_side] {
        assert_eq!(side.bots(&net.network)?[0].name, mine);
        assert!(
            side.overwritten(&net.network, "c")?.is_empty(),
            "a change over the one that stood ends it"
        );
    }
    Ok(())
}

#[test]
fn bot_changes_converge_and_retries_write_once() -> Result<()> {
    let net = net()?;
    let mut origin = store(&net)?;
    conversation(&mut origin, &net)?;
    origin.append_event(&net.a, &net.network, "c", bot(&net, "Helper")?, vec![], 2)?;
    origin.append_event(&net.a, &net.network, "c", user(&net, "m1", "x"), vec![], 3)?;
    let change = |store: &mut Store, identity, trigger, call_ref, request, at| {
        store.change_bot(
            identity,
            &net.network,
            "c",
            trigger,
            call_ref,
            request,
            Provider::Codex,
            at,
        )
    };
    let (renamed, written) = change(&mut origin, &net.a, "message:m1", "r1", rename("Nova"), 4)?;
    assert!(written);
    assert_eq!(
        renamed.change,
        BotChange::Profile {
            name: Some(Edit {
                before: "Helper".into(),
                after: "Nova".into()
            }),
            instructions: None,
            model: None,
            skills: None,
            avatar: None,

            reasoning_effort: None,
        }
    );
    assert_eq!(
        change(&mut origin, &net.a, "message:m1", "r1", rename("Nova"), 5)?,
        (renamed.clone(), false)
    );
    for (call_ref, request, error) in [
        ("r1", rename("Orion"), "different change"),
        ("r2", rename("Nova"), "nothing to change"),
    ] {
        let failed = change(&mut origin, &net.a, "message:m1", call_ref, request, 5).unwrap_err();
        assert!(failed.to_string().contains(error), "{failed}");
    }
    let (saved, _) = change(
        &mut origin,
        &net.a,
        "message:m1",
        "r3",
        save_skill(None, "Digest", "List the commits."),
        6,
    )?;
    let BotChange::Skill { skill_id, .. } = saved.change else {
        panic!("skill change")
    };
    let duplicate = change(
        &mut origin,
        &net.a,
        "message:m1",
        "r4",
        save_skill(None, "digest", "Other."),
        6,
    )
    .unwrap_err();
    assert!(duplicate.to_string().contains(&skill_id), "{duplicate}");
    let (schedule, _) = change(
        &mut origin,
        &net.a,
        "message:m1",
        "r5",
        save_schedule(None, "Morning", 1440),
        7,
    )?;
    let BotChange::Schedule { schedule_id, .. } = schedule.change else {
        panic!("schedule change")
    };

    origin.append_event(&net.b, &net.network, "c", user(&net, "m2", "y"), vec![], 8)?;
    change(&mut origin, &net.b, "message:m2", "r1", rename("Orion"), 9)?;
    change(
        &mut origin,
        &net.b,
        "message:m2",
        "r2",
        save_skill(Some(&skill_id), "Digest", "List the commits by author."),
        10,
    )?;
    change(
        &mut origin,
        &net.a,
        "message:m1",
        "r6",
        ChangeRequest::RemoveSkill {
            skill_id: skill_id.clone(),
        },
        11,
    )?;
    change(
        &mut origin,
        &net.a,
        "message:m1",
        "r7",
        save_schedule(Some(&schedule_id), "Morning", 60),
        12,
    )?;
    change(
        &mut origin,
        &net.b,
        "message:m2",
        "r3",
        ChangeRequest::RemoveSchedule {
            schedule_id: schedule_id.clone(),
        },
        13,
    )?;
    let expected = bot_projection(&origin, &net)?;
    assert_eq!(expected["bots"][0]["name"], "Orion");
    assert!(
        origin.overwritten(&net.network, "c")?.is_empty(),
        "a change that saw the one before it replaced nothing unseen"
    );
    assert_eq!(expected["skills"], serde_json::json!([]));
    assert_eq!(expected["schedules"], serde_json::json!([]));
    assert_eq!(expected["changes"].as_array().map(Vec::len), Some(8));

    let a_events = origin.events_from(&net.network, &net.a.device_id()?, 1, 100)?;
    let b_events = origin.events_from(&net.network, &net.b.device_id()?, 1, 100)?;
    let mut a_first = store(&net)?;
    a_first.ingest_events(&net.network, &a_events)?;
    a_first.ingest_events(&net.network, &b_events)?;
    let mut b_first = store(&net)?;
    b_first.ingest_events(&net.network, &b_events)?;
    b_first.ingest_events(&net.network, &a_events)?;
    b_first.ingest_events(&net.network, &b_events)?;
    assert_eq!(bot_projection(&a_first, &net)?, expected);
    assert_eq!(bot_projection(&b_first, &net)?, expected);
    Ok(())
}

#[test]
fn encrypted_session_ingest_keeps_the_source_signature_across_owners_and_retries() -> Result<()> {
    use crate::delivery::session::{SessionObjectStore, ingest_event, seal_event};
    use vibeface_delivery::Secret32;

    let net = net()?;
    let a = net.a.device_id()?;
    let b = net.b.device_id()?;
    let mut source = store(&net)?;
    bot_conversation(&mut source, &net)?;
    let owner = net.a.owner_fixture(&net.network, "d", &b)?;
    source.create_conversation(&net.b, &net.network, "d", "Destination", owner, 3)?;
    let directory = std::env::temp_dir().join(format!("vibeface-session-{}", uuid::Uuid::new_v4()));
    crate::config::private_dir(&directory)?;
    let path = directory.join("target.sqlite3");
    let mut target = Store::open(&path)?;
    target.install_trust_bundle(&net.network, &net.state)?;
    all_accepted(&mut target, &net, &source)?;

    let signed = source.append_event(
        &net.a,
        &net.network,
        "d",
        EventBody::BotMessaged {
            message_id: "relay-message-1".into(),
            from: Sender {
                conversation_id: "c".into(),
                name: "Helper".into(),
            },
            text: "private session message".into(),
            hops: 1,
        },
        vec![],
        4,
    )?;
    let key = Secret32::new([7; 32])?;
    let object = seal_event(
        &net.a,
        &net.network,
        &signed,
        hex::decode(&b)?.try_into().unwrap(),
        [8; 32],
        &key,
        4..10,
    )?;
    assert!(
        !object
            .ciphertext
            .windows(b"private session message".len())
            .any(|p| p == b"private session message")
    );
    let relay = SessionObjectStore::open(&directory)?;
    relay.put_verified(&object)?;
    let object = relay
        .get(&object.header.object_id)?
        .context("missing relay object")?;
    assert!(ingest_event(&mut target, &net.a, &net.network, &object, &key, 5).is_err());
    assert!(ingest_event(&mut target, &net.b, &net.network, &object, &key, 10).is_err());
    assert!(
        ingest_event(
            &mut target,
            &net.b,
            &net.network,
            &object,
            &Secret32::new([9; 32])?,
            5
        )
        .is_err()
    );
    let mut tampered = object.clone();
    tampered.ciphertext[0] ^= 1;
    assert!(ingest_event(&mut target, &net.b, &net.network, &tampered, &key, 5).is_err());
    assert_eq!(target.event(&net.network, &signed.hash)?, None);
    assert_eq!(
        ingest_event(&mut target, &net.b, &net.network, &object, &key, 5)?.statuses,
        [IngestStatus::Accepted]
    );
    let trigger = bot_message_trigger("relay-message-1");
    let first = target
        .admit_delivery(&b, &net.network, "d", &trigger)?
        .context("missing Bot admission")?;
    drop(target);

    let mut target = Store::open(&path)?;
    assert_eq!(target.event(&net.network, &signed.hash)?, Some(signed));
    assert_eq!(
        target
            .conversation(&net.network, "c")?
            .and_then(|c| c.owner),
        Some(a)
    );
    assert_eq!(
        target
            .conversation(&net.network, "d")?
            .and_then(|c| c.owner),
        Some(b.clone())
    );
    assert_eq!(
        ingest_event(&mut target, &net.b, &net.network, &object, &key, 6)?.statuses,
        [IngestStatus::Duplicate]
    );
    assert_eq!(target.messages(&net.network, "d")?.len(), 1);
    let second = target
        .admit_delivery(&b, &net.network, "d", &trigger)?
        .context("missing retried Bot admission")?;
    assert_eq!(first, second);
    assert_eq!(target.scheduler_jobs("d")?.len(), 1);
    drop(target);
    std::fs::remove_dir_all(directory)?;
    Ok(())
}

#[test]
fn a_bot_message_wakes_the_other_bot_once() -> Result<()> {
    let net = net()?;
    let (mut origin, _) = bot_with_message(&net)?;
    second_bot(&mut origin, &net, "Ops")?;
    let send = |store: &mut Store, call_ref: &str, to: &str, text: &str, at| {
        store.message_bot(
            &net.a,
            &net.network,
            "c",
            "message:m1",
            call_ref,
            to,
            text,
            at,
        )
    };
    let (message, receipt, written) = send(&mut origin, "s1", "ops", "Check the backups", 10)?;
    assert!(written);
    assert_eq!(message.role, Role::Bot);
    assert_eq!(
        message.sender.as_ref().map(|s| s.name.as_str()),
        Some("Helper")
    );
    let trigger = bot_message_trigger(&message.message_id);
    assert_eq!(
        (receipt.conversation_id.as_str(), receipt.trigger.as_str()),
        ("d", trigger.as_str())
    );
    let unread = origin.pending_notifications(&net.network, "web")?;
    assert!(
        unread.iter().any(|note| note.kind == "bot_message"
            && note.conversation_id == "d"
            && note.notification_id == message.event_hash),
        "the receiving chat counts the message as unread"
    );
    let (again, _, written) = send(&mut origin, "s1", "Ops", "Check the backups", 11)?;
    assert!(!written && again.message_id == message.message_id);
    let changed = send(&mut origin, "s1", "d", "Other", 11).unwrap_err();
    assert!(changed.to_string().contains("another message"), "{changed}");
    assert!(send(&mut origin, "s2", "c", "To myself", 11).is_err());
    assert_eq!(origin.scheduler_jobs("d")?.len(), 1);
    let claimed = origin
        .claim_next_fixture(&net.a, &net.network, "d", "boot", "cfg")?
        .context("bot message job")?;
    assert_eq!(
        (claimed.job.kind, claimed.job.trigger.as_str()),
        (TriggerKind::BotMessage, trigger.as_str())
    );

    let (reply, _, _) = origin.message_bot(
        &net.a,
        &net.network,
        "d",
        &trigger,
        "s1",
        "Helper",
        "Done",
        12,
    )?;
    let hops = context_after(
        &origin.connection,
        &net.network,
        "c",
        None,
        &VersionVector::new(),
    )?
    .events
    .into_iter()
    .find_map(|event| match event.event.body {
        EventBody::BotMessaged {
            message_id, hops, ..
        } if message_id == reply.message_id => Some(hops),
        _ => None,
    });
    assert_eq!(hops, Some(2));
    let sent = origin.sent_bot_messages(&net.network, "d")?;
    assert_eq!(
        sent.iter()
            .map(|row| (row.message_id.as_str(), row.to.as_str(), row.text.as_str()))
            .collect::<Vec<_>>(),
        [(reply.message_id.as_str(), "c", "Done")],
        "the sender's chat lists what it wrote to another bot"
    );

    for index in 2..=BOT_MESSAGES_PER_HOUR {
        send(
            &mut origin,
            &format!("s{index}"),
            "d",
            &format!("note {index}"),
            20,
        )?;
    }
    let limited = send(&mut origin, "s99", "d", "one more", 21).unwrap_err();
    assert!(limited.to_string().contains("last hour"), "{limited}");
    assert!(send(&mut origin, "s99", "d", "one more", 3_600_020).is_ok());

    let mut other = store(&net)?;
    all_accepted(&mut other, &net, &origin)?;
    assert!(other.scheduler_jobs("d")?.is_empty() && other.scheduler_jobs("c")?.is_empty());
    let forged = net.b.sign_event(Event {
        version: WIRE_VERSION,
        network_id: net.network.clone(),
        author: net.b.device_id()?,
        seq: 1,
        prev: None,
        deps: vec![],
        scope_id: "c".into(),
        created_at_ms: 30,
        body: EventBody::BotMessaged {
            message_id: "bot-message-x".into(),
            from: Sender {
                conversation_id: "d".into(),
                name: "Ops".into(),
            },
            text: "deep".into(),
            hops: MAX_CHAIN_DEPTH + 1,
        },
        blobs: vec![],
    });
    let forged = forged.unwrap_err();
    assert!(forged.to_string().contains("hops deep"), "{forged}");
    Ok(())
}

#[test]
fn a_bot_creates_and_configures_bots_for_the_users_message() -> Result<()> {
    let net = net()?;
    let (mut origin, m1) = bot_with_message(&net)?;
    second_bot(&mut origin, &net, "Ops")?;
    let create = |store: &mut Store, trigger: &str, call_ref: &str, name: &str| {
        store.create_bot_in_turn(
            &net.a,
            &net.network,
            "c",
            trigger,
            call_ref,
            (name, "Watch the logs.", None),
            10,
        )
    };
    let (created, new) = create(&mut origin, "message:m1", "b1", "Watcher")?;
    assert!(new);
    assert_eq!(
        create(&mut origin, "message:m1", "b1", "Watcher")?,
        (created.clone(), false)
    );
    assert!(create(&mut origin, "message:m1", "b1", "Other").is_err());
    let refused = create(&mut origin, "work:w1", "b2", "Other").unwrap_err();
    assert!(
        refused.to_string().contains(bot_settings::NOT_FROM_USER),
        "{refused}"
    );
    create(&mut origin, "message:m1", "b2", "Second")?;
    create(&mut origin, "message:m1", "b3", "Third")?;
    let capped = create(&mut origin, "message:m1", "b4", "Fourth").unwrap_err();
    assert!(capped.to_string().contains("at most"), "{capped}");
    let (changed, _) = origin.change_bot(
        &net.a,
        &net.network,
        "c",
        "message:m1",
        "k1",
        ChangeRequest::ConfigureBot {
            bot: "Ops".into(),
            name: Some("Backup".into()),
            instructions: None,
            model: None,
            skills: None,
            all_skills: None,
            avatar: None,
            default_avatar: None,

            reasoning_effort: None,
        },
        Provider::Codex,
        11,
    )?;
    assert_eq!(changed.conversation_id, "d");

    let mut other = store(&net)?;
    all_accepted(&mut other, &net, &origin)?;
    let mut names: Vec<_> = other
        .bots(&net.network)?
        .into_iter()
        .map(|bot| bot.name)
        .collect();
    names.sort();
    assert_eq!(names, ["Backup", "Helper", "Second", "Third", "Watcher"]);
    assert_eq!(
        other
            .conversation(&net.network, &created.conversation_id)?
            .and_then(|c| c.owner),
        Some(net.a.device_id()?)
    );

    let hijack = net.b.sign_event(Event {
        version: WIRE_VERSION,
        network_id: net.network.clone(),
        author: net.b.device_id()?,
        seq: 1,
        prev: None,
        deps: vec![m1],
        scope_id: "d".into(),
        created_at_ms: 12,
        body: EventBody::ConversationOwner {
            scheduler_node: net.b.device_id()?,
            epoch: 1,
            attestation: None,
            cause: Some(Cause {
                conversation_id: "c".into(),
                message_id: "m1".into(),
            }),
        },
        blobs: vec![],
    })?;
    assert!(matches!(
        &other.ingest_events(&net.network, &[hijack])?.statuses[0],
        IngestStatus::Rejected(error) if error.contains("follows its creation")
    ));
    Ok(())
}

fn configure(
    bot: &str,
    model: Option<&str>,
    skills: Option<&[&str]>,
    all_skills: Option<bool>,
) -> ChangeRequest {
    ChangeRequest::ConfigureBot {
        bot: bot.into(),
        name: None,
        instructions: None,
        model: model.map(Into::into),
        skills: skills.map(|ids| ids.iter().map(|id| id.to_string()).collect()),
        all_skills,
        avatar: None,
        default_avatar: None,

        reasoning_effort: None,
    }
}

#[test]
fn a_removed_skill_leaves_the_bots_that_used_it() -> Result<()> {
    let net = net()?;
    let (mut store, _) = bot_with_message(&net)?;
    let mut change = |call_ref: &str, request: ChangeRequest| {
        store.change_bot(
            &net.a,
            &net.network,
            "c",
            "message:m1",
            call_ref,
            request,
            Provider::Codex,
            5,
        )
    };
    let mut ids = Vec::new();
    for (call_ref, name) in [("s1", "Digest"), ("s2", "Review")] {
        let (saved, _) = change(call_ref, save_skill(None, name, "Steps."))?;
        let BotChange::Skill { skill_id, .. } = saved.change else {
            panic!("skill change")
        };
        ids.push(skill_id);
    }
    let both = [ids[0].as_str(), ids[1].as_str()];
    change("k1", configure("Helper", None, Some(&both), None))?;
    change(
        "r1",
        ChangeRequest::RemoveSkill {
            skill_id: ids[1].clone(),
        },
    )?;
    let helper = store
        .bot_for_conversation(&net.network, "c")?
        .context("helper")?;
    assert_eq!(
        helper.skills,
        Some([ids[0].clone()].into()),
        "the bot lists only skills that still exist"
    );
    let listed = crate::runtime::agent_tools::read_bot(&store, &net.network, "Helper", 100)?;
    assert_eq!(listed["skills"].as_array().map(Vec::len), Some(1));
    Ok(())
}

#[test]
fn a_bot_model_and_skills_follow_the_users_message() -> Result<()> {
    let net = net()?;
    let (mut origin, _) = bot_with_message(&net)?;
    second_bot(&mut origin, &net, "Ops")?;
    let mut change = |call_ref: &str, request: ChangeRequest| {
        origin.change_bot(
            &net.a,
            &net.network,
            "c",
            "message:m1",
            call_ref,
            request,
            Provider::Codex,
            5,
        )
    };
    let (saved, _) = change("s1", save_skill(None, "Digest", "List the commits."))?;
    let BotChange::Skill { skill_id, .. } = saved.change else {
        panic!("skill change")
    };
    change("s2", save_skill(None, "Review", "Review the diff."))?;
    let (modeled, written) = change("k1", configure("Ops", Some(" gpt-x "), None, None))?;
    assert!(written && modeled.conversation_id == "d");
    assert_eq!(
        change("k1", configure("Ops", Some("gpt-x"), None, None))?,
        (modeled, false)
    );
    let same = change("k2", configure("Ops", Some("gpt-x"), None, None)).unwrap_err();
    assert!(same.to_string().contains("nothing to change"), "{same}");
    let only = [skill_id.as_str()];
    change("k3", configure("Helper", None, Some(&only), None))?;
    let unknown = change("k4", configure("Helper", None, Some(&["skill-x"]), None)).unwrap_err();
    assert!(
        unknown.to_string().contains("skill-x not found"),
        "{unknown}"
    );
    let both = change("k5", configure("Ops", None, Some(&only), Some(true))).unwrap_err();
    assert!(both.to_string().contains("skills"), "{both}");
    change("k6", configure("Ops", None, Some(&[]), None))?;
    change("k7", configure("Ops", None, None, Some(true)))?;

    let mut other = store(&net)?;
    all_accepted(&mut other, &net, &origin)?;
    for store in [&origin, &other] {
        let bots = store.bots(&net.network)?;
        let helper = bots
            .iter()
            .find(|bot| bot.name == "Helper")
            .context("helper")?;
        let ops = bots.iter().find(|bot| bot.name == "Ops").context("ops")?;
        assert!(helper.models.is_empty());
        assert_eq!(helper.skills, Some([skill_id.clone()].into()));
        assert_eq!(ops.model(Provider::Codex), Some("gpt-x"));
        assert_eq!(ops.skills, None, "all_skills hands back every skill");
    }
    let prompt = |scope| crate::scheduler::bot_value(&other, &net.network, scope, 0);
    let (helper, ops) = (prompt("c")?, prompt("d")?);
    assert_eq!(helper["skills"].as_array().map(Vec::len), Some(1));
    assert_eq!(helper["skills"][0]["skill_id"], skill_id.as_str());
    assert_eq!(ops["skills"].as_array().map(Vec::len), Some(2));
    assert_eq!(ops["configured_bot"]["models"]["codex"], "gpt-x");
    Ok(())
}

#[test]
fn a_bot_runs_the_model_of_its_devices_provider_or_the_devices_default() -> Result<()> {
    let net = net()?;
    let (mut origin, _) = bot_with_message(&net)?;
    let change = |store: &mut Store, call_ref: &str, model: &str, provider| {
        store.change_bot(
            &net.a,
            &net.network,
            "c",
            "message:m1",
            call_ref,
            configure("Helper", Some(model), None, None),
            provider,
            5,
        )
    };
    let runs = |store: &Store, provider: Provider| -> Result<Option<String>> {
        let device = crate::config::ManagerSettings {
            provider,
            workspace: "/w".into(),
            model: "auto".into(),
            reasoning_effort: None,
            backup: Default::default(),
        };
        let bot = store.bot_for_conversation(&net.network, "c")?;
        Ok(crate::scheduler::bot_model(&device, bot.as_ref()))
    };
    let (set, _) = change(&mut origin, "k1", "gpt-x", Provider::Codex)?;
    assert!(matches!(
        set.change,
        BotChange::Profile { model: Some(ModelEdit { provider: Provider::Codex, ref before, ref after }), .. }
            if before.is_empty() && after == "gpt-x"
    ));
    let mut other = store(&net)?;
    all_accepted(&mut other, &net, &origin)?;
    assert_eq!(runs(&other, Provider::Codex)?.as_deref(), Some("gpt-x"));
    assert_eq!(
        runs(&other, Provider::Cursor)?,
        None,
        "cursor runs its default"
    );

    change(&mut origin, "k2", "composer-1", Provider::Cursor)?;
    change(&mut origin, "k3", "", Provider::Codex)?;
    let same = change(&mut origin, "k4", "", Provider::Codex).unwrap_err();
    assert!(same.to_string().contains("nothing to change"), "{same}");
    let mut later = store(&net)?;
    all_accepted(&mut later, &net, &origin)?;
    assert_eq!(
        runs(&later, Provider::Codex)?,
        None,
        "an emptied model follows the default"
    );
    assert_eq!(
        runs(&later, Provider::Cursor)?.as_deref(),
        Some("composer-1")
    );
    Ok(())
}

#[test]
fn a_deleted_bot_stops_everywhere_and_stays_deleted() -> Result<()> {
    let net = net()?;
    let (mut origin, _) = bot_with_message(&net)?;
    second_bot(&mut origin, &net, "Ops")?;
    let (created, _) = origin.create_bot_in_turn(
        &net.a,
        &net.network,
        "c",
        "message:m1",
        "b1",
        ("Watcher", "Watch the logs.", None),
        4,
    )?;
    origin.change_bot(
        &net.a,
        &net.network,
        "c",
        "message:m1",
        "r1",
        save_schedule(None, "Digest", 5),
        Provider::Codex,
        4,
    )?;
    let delete = |store: &mut Store, trigger: &str, wanted: &str| {
        store.delete_bot_in_turn(&net.a, &net.network, "c", trigger, wanted, 5)
    };
    let itself = delete(&mut origin, "message:m1", "Helper").unwrap_err();
    assert!(
        itself.to_string().contains("does not delete itself"),
        "{itself}"
    );
    let unasked = delete(&mut origin, "work:w1", "Watcher").unwrap_err();
    assert!(
        unasked.to_string().contains(bot_settings::NOT_FROM_USER),
        "{unasked}"
    );
    let (gone, event) = delete(&mut origin, "message:m1", "Watcher")?;
    assert!(gone == created.conversation_id && event.is_some());
    assert_eq!(
        delete(&mut origin, "message:m1", "Watcher")?,
        (created.conversation_id.clone(), None)
    );

    origin.post_input_fixture(
        &net.a,
        &net.network,
        signed_user(&net, "m2", "hi", true)?,
        6,
    )?;
    let signed = net
        .a
        .fixture_admin()
        .attest(
            &net.network,
            "c",
            EventBody::BotDeleted {
                attestation: None,
                cause: None,
            },
        )?
        .attestation()
        .cloned()
        .context("signed")?;
    assert!(
        origin
            .delete_bot(&net.a, &net.network, "c", signed.clone(), 7)?
            .is_some()
    );
    assert!(
        origin
            .delete_bot(&net.a, &net.network, "c", signed, 8)?
            .is_none()
    );
    assert!(
        origin
            .claim_next_fixture(&net.a, &net.network, "c", "boot", "cfg")?
            .is_none()
    );
    assert!(
        origin
            .scheduler_jobs("c")?
            .iter()
            .all(|job| matches!(job.state, JobState::Cancelled { .. }))
    );
    let refused = origin
        .post_input_fixture(
            &net.a,
            &net.network,
            signed_user(&net, "m3", "again", true)?,
            9,
        )
        .unwrap_err();
    assert!(refused.to_string().contains("deleted"), "{refused}");
    assert!(origin.schedules(&net.network, "c")?.is_empty());
    assert!(
        origin
            .admit_due_schedules(&net.a, &net.network, 10_000_000)?
            .is_empty()
    );

    let mut other = store(&net)?;
    all_accepted(&mut other, &net, &origin)?;
    for store in [&origin, &other] {
        let names: Vec<_> = store
            .bots(&net.network)?
            .into_iter()
            .map(|bot| bot.name)
            .collect();
        assert_eq!(names, ["Ops"]);
        assert_eq!(store.primary_bot(&net.network)?.as_deref(), Some("d"));
    }
    let unsigned = net.b.sign_event(Event {
        version: WIRE_VERSION,
        network_id: net.network.clone(),
        author: net.b.device_id()?,
        seq: 1,
        prev: None,
        deps: vec![],
        scope_id: "d".into(),
        created_at_ms: 11,
        body: EventBody::BotDeleted {
            attestation: None,
            cause: None,
        },
        blobs: vec![],
    });
    assert!(unsigned.is_err(), "deleting needs the user's signature");
    Ok(())
}

#[test]
fn the_users_project_choice_replaces_and_clears_a_bots_folder() -> Result<()> {
    let net = net()?;
    let mut store = store(&net)?;
    conversation(&mut store, &net)?;
    store.append_event(&net.a, &net.network, "c", bot(&net, "Helper")?, vec![], 2)?;
    let set = |project: Option<crate::model::BotProject>| {
        net.a.fixture_admin().attest(
            &net.network,
            "c",
            EventBody::BotProjectSet {
                project,
                attestation: None,
            },
        )
    };
    let project = crate::model::BotProject {
        target_node: net.a.device_id()?,
        cwd: "/srv/demo".into(),
    };
    store.append_event(
        &net.a,
        &net.network,
        "c",
        set(Some(project.clone()))?,
        vec![],
        3,
    )?;
    assert_eq!(store.bots(&net.network)?[0].project, Some(project));
    store.append_event(&net.a, &net.network, "c", set(None)?, vec![], 4)?;
    assert_eq!(store.bots(&net.network)?[0].project, None);
    let unsigned = EventBody::BotProjectSet {
        project: None,
        attestation: None,
    };
    assert!(
        store
            .append_event(&net.a, &net.network, "c", unsigned, vec![], 5)
            .is_err(),
        "only the user moves a bot's folder"
    );
    Ok(())
}

#[test]
fn a_bots_face_from_the_chat_and_from_the_panel_converge_by_rank() -> Result<()> {
    use crate::model::{Avatar, Eyes, Hue, Outline};
    use crate::store::bot_settings::AvatarParts;
    let net = net()?;
    let (mut origin, _) = bot_with_message(&net)?;
    let hue = |hue: u16| Hue::try_from(hue).map_err(anyhow::Error::msg);
    let panel = Avatar {
        outline: Outline::Bean,
        eyes: Eyes::Dot,
        hue: hue(155)?,
    };
    assert!(
        Hue::try_from(70).is_err(),
        "the halo's amber is no face hue"
    );
    assert!(
        serde_json::from_value::<ChangeRequest>(
            json!({"tool":"update_profile","avatar":{"hue":70}})
        )
        .is_err()
    );
    let parts = |outline, eyes, hue| AvatarParts { outline, eyes, hue };
    let picked = |avatar: Option<AvatarParts>, default_avatar| ChangeRequest::UpdateProfile {
        name: None,
        instructions: None,
        avatar,
        default_avatar,
    };
    let early = origin.events_from(&net.network, &net.a.device_id()?, 1, 100)?;
    let mut change = |call_ref: &str, request: ChangeRequest, at: i64| {
        origin.change_bot(
            &net.a,
            &net.network,
            "c",
            "message:m1",
            call_ref,
            request,
            Provider::Codex,
            at,
        )
    };
    let derived = Avatar::derived("bot");
    let cloud = parts(Some(Outline::Cloud), None, None);
    let (first, written) = change("f1", picked(Some(cloud), None), 4)?;
    let chat = Avatar {
        outline: Outline::Cloud,
        ..derived
    };
    assert!(written);
    assert!(
        matches!(first.change, BotChange::Profile { avatar: Some(ref edit), .. }
            if edit.before.is_none() && edit.after == Some(chat)),
        "the named part changes and the rest stays as the id gives it: {:?}",
        first.change
    );
    assert_eq!(
        change("f1", picked(Some(cloud), None), 5)?,
        (first.clone(), false),
        "the same call_ref with the same content reads back"
    );
    let other = change(
        "f1",
        picked(Some(parts(Some(Outline::Bean), None, None)), None),
        5,
    )
    .unwrap_err();
    assert!(other.to_string().contains("call_ref"), "{other}");
    let unchanged = change("f2", picked(Some(cloud), None), 5).unwrap_err();
    assert!(
        unchanged.to_string().contains("nothing to change"),
        "{unchanged}"
    );
    let empty = change("f3", picked(Some(AvatarParts::default()), None), 5).unwrap_err();
    assert!(
        empty.to_string().contains("outline, eyes or hue"),
        "{empty}"
    );
    let both = change("f4", picked(Some(cloud), Some(true)), 5).unwrap_err();
    assert!(both.to_string().contains("not both"), "{both}");
    let late = origin.events_from(
        &net.network,
        &net.a.device_id()?,
        early.len() as u64 + 1,
        100,
    )?;

    let mut panel_device = store(&net)?;
    panel_device.ingest_events(&net.network, &early)?;
    let signed = |version, avatar| {
        net.a.fixture_admin().attest(
            &net.network,
            "c",
            EventBody::BotAvatarSet {
                version,
                avatar,
                attestation: None,
            },
        )
    };
    let unsigned = EventBody::BotAvatarSet {
        version: None,
        avatar: Some(panel),
        attestation: None,
    };
    assert!(
        panel_device
            .append_event(&net.b, &net.network, "c", unsigned, vec![], 9)
            .is_err(),
        "only the user picks a face from the panel"
    );
    let mut forged = signed(None, Some(panel))?;
    if let EventBody::BotAvatarSet { version, .. } = &mut forged {
        *version = Some("0123456789abcdef".into());
    }
    assert!(
        panel_device
            .append_event(&net.b, &net.network, "c", forged, vec![], 9)
            .is_err(),
        "the signature covers the version the user saw"
    );
    panel_device.append_event(
        &net.b,
        &net.network,
        "c",
        signed(None, Some(panel))?,
        vec![],
        9,
    )?;
    let picks = panel_device.events_from(&net.network, &net.b.device_id()?, 1, 100)?;

    let mut faces = Vec::new();
    for batches in [[&early, &late, &picks], [&early, &picks, &late]] {
        let mut device = store(&net)?;
        for batch in batches {
            let statuses = device.ingest_events(&net.network, batch)?.statuses;
            assert!(
                statuses
                    .iter()
                    .all(|status| *status == IngestStatus::Accepted),
                "{statuses:?}"
            );
        }
        faces.push(
            device
                .bot_for_conversation(&net.network, "c")?
                .context("bot")?
                .avatar,
        );
    }
    assert_eq!(
        faces,
        [Some(panel), Some(panel)],
        "the later rank wins in any order"
    );

    let mut back = store(&net)?;
    all_accepted(&mut back, &net, &origin)?;
    back.ingest_events(&net.network, &picks)?;
    let (reset, _) = back.change_bot(
        &net.a,
        &net.network,
        "c",
        "message:m1",
        "f5",
        picked(None, Some(true)),
        Provider::Codex,
        20,
    )?;
    assert!(
        matches!(reset.change, BotChange::Profile { avatar: Some(ref edit), .. }
            if edit.before == Some(panel) && edit.after.is_none()),
        "default_avatar writes null: {:?}",
        reset.change
    );
    let reset_bot = back
        .bot_for_conversation(&net.network, "c")?
        .context("bot")?;
    assert_eq!(
        (reset_bot.avatar, reset_bot.avatar_version),
        (None, Some(crate::store::version(&reset.event_hash))),
        "a later chat change outranks the panel's pick and gives the face a new version"
    );
    second_bot(&mut back, &net, "Ops")?;
    let shown = back
        .bot_for_conversation(&net.network, "c")?
        .context("bot")?
        .shown_avatar();
    assert_eq!((shown.avatar, shown.picked), (derived, false));
    assert_eq!(
        crate::scheduler::bot_value(&back, &net.network, "c", 0)?["configured_bot"]["avatar"],
        json!({"outline":derived.outline,"eyes":derived.eyes,"hue":derived.hue,"picked":false}),
        "a turn sees the face the bot shows"
    );

    let (other_bot, _) = back.change_bot(
        &net.a,
        &net.network,
        "c",
        "message:m1",
        "f6",
        ChangeRequest::ConfigureBot {
            bot: "Ops".into(),
            name: None,
            instructions: None,
            model: None,
            skills: None,
            all_skills: None,
            avatar: Some(parts(None, Some(Eyes::Wide), Some(hue(285)?))),
            default_avatar: None,

            reasoning_effort: None,
        },
        Provider::Codex,
        21,
    )?;
    let ops = back
        .bot_for_conversation(&net.network, "d")?
        .context("ops")?;
    assert_eq!(other_bot.conversation_id, "d");
    assert_eq!(
        ops.avatar,
        Some(Avatar {
            eyes: Eyes::Wide,
            hue: hue(285)?,
            ..Avatar::derived("bot-d")
        }),
        "configure_bot gives another bot a face"
    );
    Ok(())
}

#[test]
fn the_node_derives_the_same_face_from_an_id_as_the_clients() -> Result<()> {
    use crate::model::Avatar;
    #[derive(serde::Deserialize)]
    struct Vector {
        bot_id: String,
        #[serde(flatten)]
        face: serde_json::Value,
    }
    let vectors: Vec<Vector> = serde_json::from_str(include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../packages/client/src/avatar/derived-faces.json"
    )))?;
    let (mut hues, mut outlines, mut eyes) = (
        std::collections::BTreeSet::new(),
        std::collections::BTreeSet::new(),
        std::collections::BTreeSet::new(),
    );
    for vector in &vectors {
        let face = Avatar::derived(&vector.bot_id);
        assert_eq!(
            serde_json::to_value(face)?,
            vector.face,
            "{}",
            vector.bot_id
        );
        hues.insert(u16::from(face.hue));
        outlines.insert(serde_json::to_string(&face.outline)?);
        eyes.insert(serde_json::to_string(&face.eyes)?);
    }
    assert_eq!(
        hues.into_iter().collect::<Vec<_>>(),
        crate::model::HUES,
        "the vectors reach every hue, so both lists agree"
    );
    assert_eq!(
        (outlines.len(), eyes.len()),
        (5, 4),
        "the vectors reach every outline and eye shape, so both orders agree"
    );
    Ok(())
}

#[test]
fn read_bot_lists_every_skill_the_prompt_index_leaves_out() -> Result<()> {
    let net = net()?;
    let (mut store, _) = bot_with_message(&net)?;
    for i in 0..40 {
        store.change_bot(
            &net.a,
            &net.network,
            "c",
            "message:m1",
            &format!("s{i}"),
            save_skill(None, &format!("skill {i:02}"), "steps"),
            Provider::Codex,
            10 + i,
        )?;
    }
    let bot = crate::scheduler::bot_value(&store, &net.network, "c", 100)?;
    let omitted = bot["skills_omitted"].as_u64().context("omitted")?;
    assert!(omitted > 0, "the prompt index stays within its budget");
    let read = crate::runtime::agent_tools::read_bot(&store, &net.network, "Helper", 100)?;
    assert_eq!(read["uses_all_skills"], json!(true));
    let names = listed(&read, "/skills", "name");
    let expected: Vec<Value> = (0..40).map(|i| json!(format!("skill {i:02}"))).collect();
    assert_eq!(
        names, expected,
        "a bot using every skill reads the whole library"
    );
    let ids = listed(&read, "/skills", "skill_id");
    assert!(
        ids.iter().all(Value::is_string),
        "each names the skill_id read_skill takes"
    );
    let chosen: std::collections::BTreeSet<String> = ids[..2]
        .iter()
        .filter_map(|id| id.as_str().map(str::to_owned))
        .collect();
    store.change_bot(
        &net.a,
        &net.network,
        "c",
        "message:m1",
        "pick",
        ChangeRequest::ConfigureBot {
            bot: "Helper".into(),
            name: None,
            instructions: None,
            model: None,
            skills: Some(chosen),
            all_skills: None,
            avatar: None,
            default_avatar: None,

            reasoning_effort: None,
        },
        Provider::Codex,
        60,
    )?;
    let read = crate::runtime::agent_tools::read_bot(&store, &net.network, "Helper", 100)?;
    assert_eq!(read["uses_all_skills"], json!(false));
    assert_eq!(
        listed(&read, "/skills", "skill_id"),
        ids[..2].to_vec(),
        "only those it uses"
    );
    Ok(())
}

#[test]
fn bot_names_stay_unique_and_the_primary_has_a_named_successor() -> Result<()> {
    let net = net()?;
    let (mut origin, _) = bot_with_message(&net)?;
    second_bot(&mut origin, &net, "Ops")?;
    assert_eq!(
        origin.primary_successor(&net.network)?.as_deref(),
        Some("d")
    );
    let taken = origin
        .create_bot_in_turn(
            &net.a,
            &net.network,
            "c",
            "message:m1",
            "n1",
            (" ops ", "", None),
            5,
        )
        .unwrap_err();
    assert!(
        taken.to_string().contains("已经有叫「ops」的 Bot"),
        "{taken}"
    );
    let renamed = origin
        .change_bot(
            &net.a,
            &net.network,
            "c",
            "message:m1",
            "r1",
            rename("OPS"),
            Provider::Codex,
            6,
        )
        .unwrap_err();
    assert!(renamed.to_string().contains("换个名字"), "{renamed}");
    Ok(())
}
