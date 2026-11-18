use super::super::test_support::{
    bot, bot_conversation, bot_reply, bot_with_message, conversation, listed, message_seq, net,
    second_bot, seq_of, store, user, work_spec, workspace_command_fixture, workspace_fixture,
    workspace_input_fixture, workspace_signed_input_fixture, workspace_task_command,
};
use super::super::*;
use crate::providers::Provider;
use crate::scheduling::message_trigger;
use crate::work::WorkSpec;
use serde_json::json;

#[test]
fn last_messages_preview_each_bots_newest_message() -> Result<()> {
    let net = net()?;
    let mut store = store(&net)?;
    conversation(&mut store, &net)?;
    store.append_event(&net.a, &net.network, "c", bot(&net, "Helper")?, vec![], 2)?;
    assert_eq!(store.last_messages(&net.network)?.get("c"), None);

    store.append_event(
        &net.a,
        &net.network,
        "c",
        user(&net, "m1", "hello"),
        vec![],
        3,
    )?;
    store.append_event(&net.a, &net.network, "c", bot_reply("a1", "m1"), vec![], 4)?;
    let last = store
        .last_messages(&net.network)?
        .remove("c")
        .expect("newest message");
    assert_eq!(
        (last.text.as_str(), last.role.as_str(), last.created_at_ms),
        ("ok", "assistant", 4)
    );

    let long = "长".repeat(200);
    store.append_event(
        &net.a,
        &net.network,
        "c",
        user(&net, "m2", &long),
        vec![],
        5,
    )?;
    let last = store
        .last_messages(&net.network)?
        .remove("c")
        .expect("newest message");
    assert_eq!(last.text, format!("{}…", "长".repeat(120)));
    assert_eq!(
        (last.role.as_str(), last.author),
        ("user", net.a.fixture_admin().device_id()?),
        "the signing device wrote it, not the entry Node"
    );
    Ok(())
}

#[test]
fn a_long_event_reads_whole_page_by_page_within_the_cap() -> Result<()> {
    let net = net()?;
    let mut store = store(&net)?;
    bot_conversation(&mut store, &net)?;
    store.append_event(&net.a, &net.network, "c", user(&net, "m1", "go"), vec![], 3)?;
    let text = "好\"\\\n\u{1}a".repeat(20_000);
    let mut reply = bot_reply("reply", "m1");
    if let EventBody::MessagePosted { text: body, .. } = &mut reply {
        *body = text.clone();
    }
    let reply = store.append_event(&net.a, &net.network, "c", reply, vec![], 10)?;
    let seq = seq_of(&reply)?;
    let author = net.a.device_id()?;
    let event = store
        .scope_event(&net.network, "c", &author, seq)?
        .context("reply")?;
    let whole = crate::scheduler::agent_event_body(&event.event.body).to_string();
    let mut read = String::new();
    let mut pages = 0;
    let mut offset = Some(0);
    while let Some(at) = offset {
        let page =
            crate::runtime::agent_tools::event_page(&store, &net.network, "c", &author, seq, at)?;
        assert!(
            page.to_string().len() <= 32_000,
            "one result stays within the cap"
        );
        read.push_str(page["body_json"].as_str().context("body_json")?);
        offset = page["next_offset"].as_u64().map(|next| next as usize);
        pages += 1;
    }
    assert!(pages > 4);
    assert_eq!(read, whole, "the pages joined are the whole body");
    let body: Value = serde_json::from_str(&read)?;
    assert_eq!(body["text"], text);
    Ok(())
}

#[test]
fn history_pages_read_at_the_turn_basis_by_position_and_type() -> Result<()> {
    let net = net()?;
    let mut store = store(&net)?;
    bot_conversation(&mut store, &net)?;
    for i in 1..=5 {
        store.append_event(
            &net.a,
            &net.network,
            "c",
            user(&net, &format!("m{i}"), &format!("t{i}")),
            vec![],
            10 + i,
        )?;
    }
    let basis = store.version_vector(&net.network)?;
    store.append_event(
        &net.a,
        &net.network,
        "c",
        user(&net, "m6", "t6"),
        vec![],
        20,
    )?;
    let author = net.a.device_id()?;
    let messages = ["input_posted".to_owned()];
    let ids = |events: Vec<HashedEvent>| -> Vec<String> {
        events
            .into_iter()
            .filter_map(|event| match event.event.body {
                EventBody::InputPosted { message_id, .. } => Some(message_id),
                _ => None,
            })
            .collect()
    };
    let (newest, more) = store.history_page(&net.network, "c", &basis, None, &messages, 2)?;
    assert_eq!(
        ids(newest),
        ["m5", "m4"],
        "the newest within the basis, not m6 that came after"
    );
    assert!(more);
    let m3 = message_seq(&store, &net, "m3")?;
    let (older, more) = store.history_page(
        &net.network,
        "c",
        &basis,
        Some((&author, m3, false)),
        &messages,
        10,
    )?;
    assert_eq!(ids(older), ["m2", "m1"]);
    assert!(!more);
    let (newer, more) = store.history_page(
        &net.network,
        "c",
        &basis,
        Some((&author, m3, true)),
        &messages,
        10,
    )?;
    assert_eq!(
        ids(newer),
        ["m4", "m5"],
        "a position stays put while new events arrive"
    );
    assert!(!more);
    let (all, _) = store.history_page(
        &net.network,
        "c",
        &basis,
        Some((&author, m3, false)),
        &[],
        10,
    )?;
    assert!(
        all.iter()
            .any(|event| !matches!(event.event.body, EventBody::InputPosted { .. })),
        "without types every event reads"
    );
    Ok(())
}

#[test]
fn history_search_filters_before_paging_and_keeps_the_recorded_basis() -> Result<()> {
    let net = net()?;
    let mut store = store(&net)?;
    bot_conversation(&mut store, &net)?;
    for (id, author, text) in [
        ("s1", &net.a, "Needle 100%_ 中文"),
        ("s2", &net.b, "needle from B"),
        ("s3", &net.a, "unrelated text"),
        ("s4", &net.a, "needle again"),
    ] {
        store.append_event(author, &net.network, "c", user(&net, id, text), vec![], 10)?;
    }
    let basis = store.version_vector(&net.network)?;
    store.append_event(
        &net.a,
        &net.network,
        "c",
        user(&net, "s5", "needle after basis"),
        vec![],
        11,
    )?;
    let a = net.a.device_id()?;
    let b = net.b.device_id()?;
    let kinds = ["input_posted".into()];
    let filter = || crate::store::HistoryFilter {
        kinds: &kinds,
        author: Some(&a),
        keyword: Some("NEEDLE"),
    };
    let (newest, more) = store.search_history_page(&net.network, "c", &basis, None, filter(), 1)?;
    assert_eq!(newest.len(), 1);
    assert_eq!(newest[0].event.body.subject(), Some("s4"));
    assert!(more);
    let seq = newest[0].event.seq;
    let (older, more) = store.search_history_page(
        &net.network,
        "c",
        &basis,
        Some((&a, seq, false)),
        filter(),
        1,
    )?;
    assert_eq!(older[0].event.body.subject(), Some("s1"));
    assert!(
        !more,
        "nonmatching records cannot invent another matching page"
    );
    let (newer, more) = store.search_history_page(
        &net.network,
        "c",
        &basis,
        Some((&a, older[0].event.seq, true)),
        filter(),
        10,
    )?;
    assert_eq!(newer.len(), 1);
    assert_eq!(newer[0].event.body.subject(), Some("s4"));
    assert!(!more, "events beyond the turn basis are invisible");
    for keyword in ["100%_", "中文"] {
        let (found, more) = store.search_history_page(
            &net.network,
            "c",
            &basis,
            None,
            crate::store::HistoryFilter {
                kinds: &kinds,
                author: None,
                keyword: Some(keyword),
            },
            1,
        )?;
        assert_eq!(found[0].event.body.subject(), Some("s1"));
        assert!(!more);
    }
    let (found, more) = store.search_history_page(
        &net.network,
        "c",
        &basis,
        None,
        crate::store::HistoryFilter {
            kinds: &kinds,
            author: Some(&b),
            keyword: Some("needle"),
        },
        1,
    )?;
    assert_eq!(found[0].event.body.subject(), Some("s2"));
    assert!(!more);
    let (missing, more) =
        store.search_history_page(&net.network, "different-scope", &basis, None, filter(), 1)?;
    assert!(missing.is_empty() && !more);
    Ok(())
}

#[test]
fn an_attachment_page_reads_text_as_text_and_splits_on_characters() -> Result<()> {
    let net = net()?;
    let mut store = store(&net)?;
    bot_conversation(&mut store, &net)?;
    let attach = |store: &mut Store, id: &str, content: &[u8], time| -> Result<String> {
        let file = Attachment {
            hash: crate::crypto::digest(content),
            size: content.len() as u64,
            name: format!("{id}.bin"),
        };
        let message = net.a.fixture_admin().attest(
            &net.network,
            "c",
            EventBody::InputPosted {
                message_id: id.into(),
                target: crate::workspace::InputTarget::BotMain {
                    bot_id: "bot".into(),
                },
                intent: crate::workspace::InputIntent::Continue,
                sequence: 1,
                expected_revision: None,
                text: id.into(),
                attachments: vec![file.clone()],
                report_results: Some(true),
                attestation: None,
            },
        )?;
        store.put_blob(&file.blob(), content)?;
        store.append_event(&net.a, &net.network, "c", message, vec![file.blob()], time)?;
        Ok(file.hash)
    };
    let text = "好".repeat(1_000);
    let hash = attach(&mut store, "m1", text.as_bytes(), 10)?;
    let binary = attach(&mut store, "m2", &[0xff, 0x00, 0xfe], 11)?;
    let page = |hash: &str, offset: u64| {
        crate::runtime::agent_tools::blob_page(
            &store,
            &net.network,
            "c",
            hash,
            &json!({"offset":offset,"limit":1}),
        )
    };
    let mut read = String::new();
    let mut offset = Some(0);
    while let Some(at) = offset {
        let chunk = page(&hash, at)?;
        read.push_str(chunk["text"].as_str().context("UTF-8 comes back as text")?);
        offset = chunk["next_offset"].as_u64();
    }
    assert_eq!(
        read, text,
        "pages split between characters, never inside one"
    );
    assert_eq!(page(&binary, 0)?["data_base64"], "/wD+");
    std::fs::remove_file(store.blob_file(&binary)?)?;
    assert_eq!(page(&binary, 0)?["pending_download"], true);
    Ok(())
}

#[test]
fn work_status_pages_newest_first_within_a_byte_cap() -> Result<()> {
    let net = net()?;
    let mut store = store(&net)?;
    conversation(&mut store, &net)?;
    for i in 0..30 {
        let spec = WorkSpec {
            work_id: format!("w{i:02}"),
            prompt: "p".repeat(2_000),
            ..work_spec(
                &net.a.device_id()?,
                &message_trigger("m"),
                &format!("call-{i}"),
                Provider::Codex,
            )
        };
        store.accept_work_fixture(&net.a, &net.network, spec, 10 + i)?;
    }
    let works = store.works(&net.network, "c")?;
    let mut seen = Vec::new();
    let mut before: Option<String> = None;
    loop {
        let page = crate::runtime::agent_tools::work_status(
            &works,
            &[],
            &json!({"before":before,"limit":50}),
        )?
        .value;
        assert!(
            page.to_string().len() <= 33_000,
            "a page stays within the cap"
        );
        let ids = listed(&page, "/works", "work_id");
        before = ids.last().and_then(|id| id.as_str()).map(str::to_owned);
        seen.extend(ids);
        if page["more"] != true {
            break;
        }
    }
    let expected: Vec<Value> = (0..30).rev().map(|i| json!(format!("w{i:02}"))).collect();
    assert_eq!(seen, expected, "every work once, newest first");
    Ok(())
}

#[test]
fn list_sessions_pages_every_session_with_the_version_a_hand_over_needs() -> Result<()> {
    let net = net()?;
    let (mut store, _) = bot_with_message(&net)?;
    second_bot(&mut store, &net, "Other")?;
    let node = net.a.device_id()?;
    for i in 0..40 {
        store.claim_session(
            &net.a,
            &net.network,
            "c",
            &Subject {
                node: node.clone(),
                agent: Some(Provider::Codex),
                session_id: format!("s{i:02}"),
                cwd: "/work/project".into(),
                summary: "tests fail again. ".repeat(6),
                request_id: None,
            },
            10 + i,
        )?;
    }
    let bot = crate::scheduler::bot_value(&store, &net.network, "c", 100)?;
    let shown = listed(&bot, "/sessions", "session_id");
    assert!(
        !shown.contains(&json!("s00")),
        "the oldest falls out of the prompt"
    );
    store.claim_session(
        &net.a,
        &net.network,
        "d",
        &Subject {
            node: node.clone(),
            agent: Some(Provider::Codex),
            session_id: "theirs".into(),
            cwd: "/work/project".into(),
            summary: String::new(),
            request_id: None,
        },
        60,
    )?;
    let mut seen = Vec::new();
    let mut before = Value::Null;
    loop {
        let sessions = store.attributed_sessions(&net.network, "c")?;
        let page = crate::runtime::agent_tools::list_sessions(
            &sessions,
            &json!({"before":before,"limit":7}),
        )?;
        let rows = page["sessions"].as_array().context("sessions")?.clone();
        if before.is_null() {
            let last = rows.last().context("a page has rows")?;
            let version = last["version"].as_str().context("version")?;
            let session = last["session_id"].as_str().context("session")?;
            store.hand_over_session(&net.a, &net.network, "c", session, version, Some("d"), 70)?;
        }
        seen.extend(rows);
        if page["more"] != true {
            break;
        }
        before = page["next"].clone();
    }
    let ids: Vec<Value> = seen.iter().map(|row| row["session_id"].clone()).collect();
    let expected: Vec<Value> = (0..40).rev().map(|i| json!(format!("s{i:02}"))).collect();
    assert_eq!(
        ids, expected,
        "every session once, newest first, though one moved between pages"
    );
    let oldest = seen.last().context("oldest")?;
    let version = oldest["version"].as_str().context("version")?;
    let (_, _, written) =
        store.hand_over_session(&net.a, &net.network, "c", "s00", version, Some("d"), 80)?;
    assert!(
        written.is_some(),
        "a session left out of the prompt is handed over"
    );
    Ok(())
}

#[test]
fn workspace_history_pages_and_message_search_cover_committed_scoped_text() -> Result<()> {
    use crate::workspace::*;
    use std::collections::BTreeSet;
    use vibeface_protocol::message_search::*;
    let (net, mut origin) = workspace_fixture()?;
    workspace_command_fixture(
        &net,
        &mut origin,
        "channel",
        WorkspaceCommand::CreateChannel {
            channel_id: "channel".into(),
            title: "Channel".into(),
            purpose: "search".into(),
            members: vec!["bot".into()],
            coordinator_bot: "bot".into(),
            coordinator_node: net.a.device_id()?,
        },
        3,
    )?;
    for task in ["task-one", "task-two"] {
        let mut command = workspace_task_command(task, None);
        if let WorkspaceCommand::CreateTask { channel_id, .. } = &mut command {
            *channel_id = Some("channel".into());
        }
        workspace_command_fixture(&net, &mut origin, task, command, 4)?;
    }
    for i in 0..8 {
        let task = if i % 2 == 0 { "task-one" } else { "task-two" };
        let mut input = InputRequest {
            message_id: format!("message-{i}"),
            route: InputRoute {
                target: InputTarget::Task {
                    task_id: task.into(),
                },
                intent: InputIntent::Continue,
                sequence: i + 1,
                expected_revision: Some(1),
            },
            text: format!("中文 Needle %_\"' 第{i}条"),
            attachments: vec![],
            report_results: true,
        };
        workspace_signed_input_fixture(&net, &mut origin, &net.a, input.clone(), 5)?;
        input.message_id = format!("main-{i}");
        input.route.target = InputTarget::BotMain {
            bot_id: "bot".into(),
        };
        input.route.expected_revision = None;
        workspace_signed_input_fixture(&net, &mut origin, &net.a, input, 5)?;
    }
    workspace_command_fixture(
        &net,
        &mut origin,
        "channel-post",
        WorkspaceCommand::PostChannelMessage {
            channel_id: "channel".into(),
            message_id: "post".into(),
            text: "中文 Needle %_\"' 频道".into(),
            attachments: vec![],
            recipient_bot: None,
            task_id: None,
            reply_to: None,
        },
        6,
    )?;
    for (id, input) in [("reply-task", "message-0"), ("main-reply", "main-0")] {
        let mut reply = bot_reply(id, input);
        if let EventBody::MessagePosted { text, .. } = &mut reply {
            *text = "中文 Needle %_\"' 回复".into();
        }
        origin.append_event(&net.a, &net.network, "c", reply, vec![], 7)?;
    }
    let before = origin.version_vector(&net.network)?;
    for (channel, task) in [
        (None, Some("task-one")),
        (Some("channel"), None),
        (None, None),
    ] {
        let expected = origin
            .workspace_history(&net.network, channel, task, None, 512)?
            .events;
        let mut cursor = None;
        let mut collected = Vec::new();
        loop {
            let page =
                origin.workspace_history(&net.network, channel, task, cursor.as_deref(), 2)?;
            assert_eq!(page.complete, page.next_cursor.is_none());
            assert_eq!(page.coverage.truncated, !page.complete);
            let mut older = page.events;
            older.extend(collected);
            collected = older;
            cursor = page.next_cursor;
            if cursor.is_none() {
                break;
            }
        }
        assert_eq!(collected, expected);
        let ids: BTreeSet<_> = collected.iter().map(|event| &event.event_hash).collect();
        assert_eq!(ids.len(), collected.len());
    }
    let first = origin.workspace_history(&net.network, None, Some("task-one"), None, 2)?;
    let cursor = first.next_cursor.context("more task history")?;
    assert!(
        origin
            .workspace_history(&net.network, None, Some("task-two"), Some(&cursor), 2)
            .is_err()
    );
    assert!(
        origin
            .workspace_history(&net.network, Some("channel"), None, Some(&cursor), 2)
            .is_err()
    );
    assert!(
        origin
            .workspace_history(
                &net.b.fixture_network_id()?,
                None,
                Some("task-one"),
                Some(&cursor),
                2
            )
            .is_err()
    );
    assert!(
        origin
            .workspace_history(&net.network, None, Some("task-one"), None, 0)
            .is_err()
    );

    let search = |scope: MessageSearchScope, query: &str, cursor: Option<String>| {
        origin.message_search(
            &net.network,
            &MessageSearchRequest {
                scope,
                query: query.into(),
                cursor,
                limit: 2,
            },
        )
    };
    let main = MessageSearchScope::BotMain {
        bot_id: "bot".into(),
    };
    let task = MessageSearchScope::Task {
        task_id: "task-one".into(),
    };
    let channel = MessageSearchScope::Channel {
        channel_id: "channel".into(),
    };
    for (scope, count) in [(main.clone(), 9), (task.clone(), 5), (channel.clone(), 10)] {
        let mut ids = BTreeSet::new();
        let mut cursor = None;
        loop {
            let page = search(scope.clone(), "中文 needle %_\"'", cursor)?;
            assert_eq!(page.scope, scope);
            assert_eq!(page.complete, page.next_cursor.is_none());
            for item in &page.items {
                assert!(ids.insert(item.event_hash.clone()));
                assert!(!item.text.is_empty());
                assert!(item.rank.ends_with(&item.event_hash));
                if scope == main {
                    assert!(item.message_id.starts_with("main-"));
                } else {
                    assert!(!item.message_id.starts_with("main-"));
                }
            }
            cursor = page.next_cursor;
            if cursor.is_none() {
                break;
            }
        }
        assert_eq!(ids.len(), count);
    }
    let first = search(main.clone(), "中文", None)?;
    let cursor = first.next_cursor.context("search continuation")?;
    assert!(search(task, "中文", Some(cursor.clone())).is_err());
    assert!(search(main.clone(), "Needle", Some(cursor.clone())).is_err());
    assert!(
        origin
            .message_search(
                &net.b.fixture_network_id()?,
                &MessageSearchRequest {
                    scope: main.clone(),
                    query: "中文".into(),
                    cursor: Some(cursor),
                    limit: 2
                }
            )
            .is_err()
    );
    for query in ["message_id", "task-one", "target", "absent"] {
        let page = search(channel.clone(), query, None)?;
        assert!(page.items.is_empty() && page.complete && page.next_cursor.is_none());
    }
    assert!(search(main, " ", None).is_err());
    assert_eq!(
        origin.version_vector(&net.network)?,
        before,
        "reads never append events or replay effects"
    );
    let scope = MessageSearchScope::Task {
        task_id: "task-one".into(),
    };
    let first = origin.message_search(
        &net.network,
        &MessageSearchRequest {
            scope: scope.clone(),
            query: "中文".into(),
            cursor: None,
            limit: 2,
        },
    )?;
    let mut replacement = bot_reply("reply-task", "message-0");
    if let EventBody::MessagePosted { text, .. } = &mut replacement {
        *text = "已替换的中文正文".into();
    }
    let losing = origin.append_event(&net.a, &net.network, "c", replacement, vec![], 8)?;
    assert!(
        origin
            .message_search(
                &net.network,
                &MessageSearchRequest {
                    scope: scope.clone(),
                    query: "中文".into(),
                    cursor: first.next_cursor,
                    limit: 2
                }
            )
            .is_err(),
        "replacement expires the task search authority"
    );
    let superseded = origin.message_search(
        &net.network,
        &MessageSearchRequest {
            scope: scope.clone(),
            query: "已替换".into(),
            cursor: None,
            limit: 100,
        },
    )?;
    assert!(
        superseded.items.is_empty(),
        "losing competing reply text is not a visible search hit"
    );
    let projected = origin.workspace_history(&net.network, None, Some("task-one"), None, 100)?;
    let winner = origin
        .message(&net.network, "c", "reply-task")?
        .context("visible reply")?;
    assert!(
        !projected
            .events
            .iter()
            .any(|event| event.event_hash == losing.hash)
    );
    let visible_reply = projected
        .events
        .iter()
        .find(|event| event.event_hash == winner.event_hash)
        .context("winner in UI history")?;
    assert!(
        matches!(&visible_reply.body, EventBody::MessagePosted { text, .. } if text == &winner.text)
    );
    let search_winner = origin.message_search(
        &net.network,
        &MessageSearchRequest {
            scope,
            query: "回复".into(),
            cursor: None,
            limit: 100,
        },
    )?;
    assert_eq!(search_winner.items.len(), 1);
    assert_eq!(search_winner.items[0].event_hash, winner.event_hash);
    assert_eq!(search_winner.items[0].text, winner.text);
    assert!(
        origin
            .events_from(&net.network, &net.a.device_id()?, 1, 100)?
            .iter()
            .any(|event| event.hash == losing.hash),
        "the raw DAG retains the competing event"
    );

    Ok(())
}

#[test]
fn workspace_history_authority_expires_overlapping_windows_after_fork_reprojection() -> Result<()> {
    use crate::workspace::*;
    let (net, mut origin) = workspace_fixture()?;
    workspace_command_fixture(
        &net,
        &mut origin,
        "create-task",
        workspace_task_command("task", None),
        3,
    )?;
    let base = workspace_input_fixture(
        &net,
        &mut origin,
        "base",
        Some("task"),
        InputIntent::Continue,
        4,
    )?;
    let mut fork = store(&net)?;
    fork.ingest_events(
        &net.network,
        &origin.events_from(&net.network, &net.a.device_id()?, 1, 100)?,
    )?;
    let discarded = workspace_input_fixture(
        &net,
        &mut origin,
        "branch-a",
        Some("task"),
        InputIntent::Continue,
        5,
    )?;
    let first = origin.workspace_history(&net.network, None, Some("task"), None, 2)?;
    let old_window = origin.workspace_history(&net.network, None, Some("task"), None, 512)?;
    assert!(old_window.complete);
    let cursor = first.next_cursor.context("older task history")?;
    workspace_input_fixture(
        &net,
        &mut origin,
        "tail",
        Some("task"),
        InputIntent::Continue,
        6,
    )?;
    let appended = origin.workspace_history(&net.network, None, Some("task"), None, 2)?;
    assert_eq!(first.history_authority, appended.history_authority);
    let older = origin.workspace_history(&net.network, None, Some("task"), Some(&cursor), 2)?;
    assert_eq!(older.history_authority, first.history_authority);
    assert!(older.complete);
    let conflicting = workspace_input_fixture(
        &net,
        &mut fork,
        "branch-b",
        Some("task"),
        InputIntent::Continue,
        5,
    )?;
    let envelope = fork
        .events_from(&net.network, &net.a.device_id()?, 1, 100)?
        .into_iter()
        .find(|event| event.hash == conflicting.source_event)
        .context("conflicting input event")?;
    origin.ingest_events(&net.network, &[envelope])?;
    let refreshed = origin.workspace_history(&net.network, None, Some("task"), None, 2)?;
    assert_ne!(first.history_authority, refreshed.history_authority);
    assert!(refreshed.complete);
    assert!(
        refreshed
            .events
            .iter()
            .any(|event| event.event_hash == base.source_event)
    );
    assert!(
        old_window
            .events
            .iter()
            .any(|event| event.event_hash == base.source_event)
    );
    assert!(
        old_window
            .events
            .iter()
            .any(|event| event.event_hash == discarded.source_event)
    );
    assert!(
        !refreshed
            .events
            .iter()
            .any(|event| event.event_hash == discarded.source_event)
    );
    assert!(
        origin
            .workspace_history(&net.network, None, Some("task"), Some(&cursor), 2)
            .is_err()
    );
    Ok(())
}
