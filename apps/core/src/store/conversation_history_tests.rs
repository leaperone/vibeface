use super::*;
use vibeface_protocol::history::{HISTORY_PAGE_BYTES, HISTORY_RESPONSE_BYTES};

pub(super) fn paged(store: &Store, network: &str, limit: usize) -> Result<Vec<Message>> {
    let mut cursor = None;
    let mut all = Vec::new();
    for _ in 0..10_000 {
        let page = store.conversation_messages_page(network, "c", cursor.as_deref(), limit)?;
        assert!(serde_json::to_vec(&page)?.len() < HISTORY_RESPONSE_BYTES);
        assert!(page.items.len() <= limit);
        assert_eq!(page.complete, page.next_cursor.is_none());
        all.splice(0..0, page.items);
        cursor = page.next_cursor;
        if cursor.is_none() {
            return Ok(all);
        }
    }
    bail!("history cursor did not terminate")
}

#[test]
fn signed_long_message_pages_preserve_content_and_new_arrivals() -> Result<()> {
    let net = net()?;
    let mut origin = store(&net)?;
    bot_conversation(&mut origin, &net)?;
    assert!(paged(&origin, &net.network, 17)?.is_empty());
    for index in 0..1024 {
        let text = format!("{index}:{}", "历史\\\"\n".repeat(1024));
        origin.append_event(&net.a, &net.network, "c", user(&net, &format!("m{index}"), &text), vec![], 3 + index)?;
    }
    let oracle = origin.main_messages(&net.network, "c")?;
    assert!(serde_json::to_vec(&oracle)?.len() > 4 * 1024 * 1024);
    let first = origin.conversation_messages_page(&net.network, "c", None, 100)?;
    assert!(serde_json::to_vec(&first)?.len() < HISTORY_PAGE_BYTES + 8192);
    let cursor = first.next_cursor.as_deref().context("older history")?;
    assert!(origin.conversation_messages_page(&net.network, "other", Some(cursor), 100).is_err());
    assert!(origin.conversation_messages_page("wrong-network", "c", Some(cursor), 100).is_err());
    assert!(origin.conversation_messages_page(&net.network, "c", None, 0).is_err());
    assert!(origin.conversation_messages_page(&net.network, "c", None, 101).is_err());
    assert_eq!(paged(&origin, &net.network, 100)?, oracle);
    origin.append_event(&net.a, &net.network, "c", user(&net, "new-arrival", "new after page"), vec![], 2000)?;
    let mut older = origin.conversation_messages_page(&net.network, "c", Some(cursor), 100)?;
    let mut collected = first.items;
    loop {
        collected.splice(0..0, older.items);
        let Some(cursor) = older.next_cursor else { break };
        older = origin.conversation_messages_page(&net.network, "c", Some(&cursor), 100)?;
    }
    assert_eq!(collected, oracle, "an append does not shift the older page boundary");
    assert_eq!(paged(&origin, &net.network, 100)?, origin.main_messages(&net.network, "c")?);
    Ok(())
}

#[test]
fn signed_scoped_pages_match_full_oracle_and_allow_empty_continuations() -> Result<()> {
    use crate::workspace::*;
    let (net, mut origin) = workspace_fixture()?;
    workspace_command_fixture(&net, &mut origin, "create-task", workspace_task_command("task", None), 3)?;
    workspace_input_fixture(&net, &mut origin, "main", None, InputIntent::Continue, 4)?;
    for index in 0..8 {
        let id = format!("task-{index}");
        workspace_input_fixture(&net, &mut origin, &id, Some("task"), InputIntent::Continue, 5 + index * 2)?;
        origin.append_event(&net.a, &net.network, "c", bot_reply(&format!("reply-{index}"), &id), vec![], 6 + index * 2)?;
    }
    let first = origin.conversation_messages_page(&net.network, "c", None, 2)?;
    assert!(first.items.is_empty());
    assert!(first.next_cursor.is_some());
    assert_eq!(paged(&origin, &net.network, 2)?, origin.main_messages(&net.network, "c")?);
    let mut replica = store(&net)?;
    let events = origin.events_from(&net.network, &net.a.device_id()?, 1, 100)?;
    for event in events.iter().rev() {
        replica.ingest_events(&net.network, std::slice::from_ref(event))?;
    }
    assert_eq!(paged(&replica, &net.network, 3)?, origin.main_messages(&net.network, "c")?);
    Ok(())
}

#[test]
fn history_cursor_rejects_fork_rebuild() -> Result<()> {
    let net = net()?;
    let mut first = store(&net)?;
    bot_conversation(&mut first, &net)?;
    first.append_event(&net.a, &net.network, "c", user(&net, "base", "base"), vec![], 3)?;
    let mut second = store(&net)?;
    second.ingest_events(&net.network, &first.events_from(&net.network, &net.a.device_id()?, 1, 100)?)?;
    first.append_event(&net.a, &net.network, "c", user(&net, "branch-a", "branch a"), vec![], 4)?;
    let cursor = first.conversation_messages_page(&net.network, "c", None, 1)?.next_cursor.context("cursor")?;
    let fork = second.append_event(&net.a, &net.network, "c", user(&net, "branch-b", "branch b"), vec![], 4)?;
    first.ingest_events(&net.network, &[fork])?;
    assert!(first.conversation_messages_page(&net.network, "c", Some(&cursor), 1).is_err());
    assert_eq!(paged(&first, &net.network, 1)?, first.main_messages(&net.network, "c")?);
    Ok(())
}

#[test]
fn a_large_signed_reply_is_returned_whole_beyond_the_soft_page_budget() -> Result<()> {
    let net = net()?;
    let mut origin = store(&net)?;
    bot_conversation(&mut origin, &net)?;
    origin.append_event(&net.a, &net.network, "c", user(&net, "request", "answer fully"), vec![], 3)?;
    let text = "complete reply ".repeat(48_000);
    let mut reply = bot_reply("large-reply", "request");
    if let EventBody::MessagePosted { text: reply_text, .. } = &mut reply { *reply_text = text.clone(); }
    origin.append_event(&net.a, &net.network, "c", reply, vec![], 4)?;
    let page = origin.conversation_messages_page(&net.network, "c", None, 100)?;
    assert_eq!(page.items.len(), 1);
    assert_eq!(page.items[0].text, text);
    assert!(serde_json::to_vec(&page)?.len() > HISTORY_PAGE_BYTES);
    assert!(serde_json::to_vec(&page)?.len() < HISTORY_RESPONSE_BYTES);
    assert!(page.next_cursor.is_some());
    assert_eq!(paged(&origin, &net.network, 100)?, origin.main_messages(&net.network, "c")?);
    let initial = origin.conversation_history_start_cursor(&net.network, "c", vibeface_protocol::history::HistorySection::Messages)?;
    let deferred = origin.conversation_messages_page(&net.network, "c", Some(&initial), 100)?;
    assert_eq!(page.items, deferred.items);
    assert_eq!(page.next_cursor, deferred.next_cursor);
    assert_eq!(page.history_authority, deferred.history_authority);
    Ok(())
}

#[test]
fn routine_reply_pages_keep_the_same_scope_truth_as_the_full_projection() -> Result<()> {
    let net = net()?;
    let (mut origin, mut replica, schedule) = scheduled_on_both(&net, 60)?;
    origin.append_event(&net.a, &net.network, "c",
        bot_reply("routine-answer", &crate::scheduling::schedule_trigger(&schedule, 3_600_000)), vec![], 4)?;
    origin.append_event(&net.a, &net.network, "c", bot_reply("main-answer", "m1"), vec![], 5)?;
    let oracle = origin.main_messages(&net.network, "c")?;
    assert_eq!(oracle.iter().map(|message| message.message_id.as_str()).collect::<Vec<_>>(), ["m1", "main-answer"]);
    assert_eq!(paged(&origin, &net.network, 1)?, oracle);
    replica.ingest_events(&net.network, &origin.events_from(&net.network, &net.a.device_id()?, 1, 100)?)?;
    assert_eq!(paged(&replica, &net.network, 1)?, oracle);
    Ok(())
}

#[test]
fn settings_deletion_invalidates_loaded_history_but_tail_messages_do_not() -> Result<()> {
    let net = net()?;
    let (mut origin, _, schedule) = scheduled_on_both(&net, 60)?;
    let authority = origin.conversation_history_authority(&net.network, "c")?;
    let cursor = origin.conversation_history_start_cursor(&net.network, "c", vibeface_protocol::history::HistorySection::Schedules)?;
    origin.append_event(&net.a, &net.network, "c", user(&net, "tail", "new tail"), vec![], 4)?;
    origin.append_event(&net.a, &net.network, "c", bot_reply("tail-reply", "tail"), vec![], 5)?;
    assert_eq!(origin.conversation_history_authority(&net.network, "c")?, authority);
    let delete = net.a.fixture_admin().attest(&net.network, "c", EventBody::ScheduleControlled {
        schedule_id: schedule, action: ScheduleAction::Delete, attestation: None,
    })?;
    origin.append_event(&net.a, &net.network, "c", delete, vec![], 6)?;
    assert_ne!(origin.conversation_history_authority(&net.network, "c")?, authority);
    assert!(origin.conversation_schedules_page(&net.network, "c", Some(&cursor), 50).is_err());
    assert!(origin.conversation_schedules_page(&net.network, "c", None, 50)?.items.is_empty());
    Ok(())
}

#[test]
fn assigning_a_loaded_main_work_to_a_task_invalidates_the_old_projection() -> Result<()> {
    use crate::workspace::*;
    let (net, mut origin) = workspace_fixture()?;
    workspace_input_fixture(&net, &mut origin, "m1", None, InputIntent::Continue, 3)?;
    let work = isolated_task(&mut origin, &net, "moving-work")?;
    workspace_command_fixture(&net, &mut origin, "create-private-task", workspace_task_command("private-task", None), 5)?;
    let page = origin.conversation_works_page(&net.network, "c", None, 50)?;
    assert_eq!(page.items.iter().map(|work| work.work_id.as_str()).collect::<Vec<_>>(), ["moving-work"]);
    let cursor = origin.conversation_history_start_cursor(&net.network, "c", vibeface_protocol::history::HistorySection::Works)?;
    workspace_command_fixture(&net, &mut origin, "attach-work", WorkspaceCommand::AssignTask {
        task_id: "private-task".into(), expected_revision: 1, assignment_id: "private-assignment".into(),
        bot_id: "bot".into(), execution_node: net.a.device_id()?, bot_owner_epoch: 1,
        channel_owner_epoch: None, work_id: Some(work.work_id), resource_roots: vec![], automatic: false,
    }, 6)?;
    let current = origin.conversation_works_page(&net.network, "c", None, 50)?;
    assert_ne!(current.history_authority, page.history_authority);
    assert!(current.items.is_empty(), "the previously loaded work now belongs to the task");
    assert!(origin.conversation_works_page(&net.network, "c", Some(&cursor), 50).is_err());
    Ok(())
}

#[test]
fn a_late_parent_invalidates_a_window_that_previously_excluded_its_reply() -> Result<()> {
    let net = net()?;
    let mut origin = store(&net)?;
    bot_conversation(&mut origin, &net)?;
    origin.append_event(&net.a, &net.network, "c", bot_reply("early-reply", "late-source"), vec![], 3)?;
    let before = origin.conversation_messages_page(&net.network, "c", None, 50)?;
    assert!(before.items.is_empty());
    origin.append_event(&net.a, &net.network, "c", user(&net, "late-source", "now known"), vec![], 4)?;
    let after = origin.conversation_messages_page(&net.network, "c", None, 50)?;
    assert_ne!(before.history_authority, after.history_authority);
    assert_eq!(after.items.iter().map(|message| message.message_id.as_str()).collect::<Vec<_>>(), ["early-reply", "late-source"]);
    assert_eq!(after.items, origin.main_messages(&net.network, "c")?);
    Ok(())
}

#[test]
fn rebuilding_identical_business_rows_never_reuses_a_history_authority() -> Result<()> {
    let net = net()?;
    let mut origin = store(&net)?;
    bot_conversation(&mut origin, &net)?;
    origin.append_event(&net.a, &net.network, "c", user(&net, "before-replay", "unchanged content"), vec![], 3)?;
    let page = origin.conversation_messages_page(&net.network, "c", None, 50)?;
    let cursor = origin.conversation_history_start_cursor(&net.network, "c", vibeface_protocol::history::HistorySection::Messages)?;
    performance_tests::assert_full_recompute_oracle(&origin, &net.network)?;
    let rebuilt = origin.conversation_messages_page(&net.network, "c", None, 50)?;
    assert_eq!(rebuilt.items, page.items);
    assert_ne!(rebuilt.history_authority, page.history_authority);
    assert!(origin.conversation_messages_page(&net.network, "c", Some(&cursor), 50).is_err());
    Ok(())
}

#[test]
fn maximal_signed_work_events_measure_the_complete_history_response() -> Result<()> {
    let net = net()?;
    let mut origin = store(&net)?;
    bot_conversation(&mut origin, &net)?;
    origin.append_event(&net.a, &net.network, "c", user(&net, "m1", "full work"), vec![], 3)?;
    isolated_task(&mut origin, &net, "older-work")?;
    let mut next_cursor = None;
    let mut prefix = origin.events_from(&net.network, &net.a.device_id()?, 1, 100)?;
    let bodies = [
        EventBody::WorkDispatched {
            work_id: "maximal-work".into(), trigger: "message:m1".into(), dispatcher: net.a.device_id()?,
            call_ref: "maximal-work".into(), kind: WorkKind::Task, model: None,
            resolved: ResolvedModel { provider: Provider::Codex, model: None }, cwd: "/tmp".into(),
            title: "complete content".into(), prompt: String::new(), continues: None, parent: None, thread: None,
        },
        EventBody::WorkProgress { work_id: "maximal-work".into(), phase: WorkPhase::Running,
            summary: String::new(), thread_id: None, turn_id: None },
        EventBody::WorkFinished { work_id: "maximal-work".into(), outcome: WorkOutcome::Completed,
            result_text: String::new(), failure_reason: None, failure: None, attestation: None },
    ];
    for (index, mut body) in bodies.into_iter().enumerate() {
        let mut probe = store(&net)?;
        probe.ingest_events(&net.network, &prefix)?;
        let empty = probe.append_event(&net.a, &net.network, "c", body.clone(), vec![], 4 + index as i64)?;
        let text = "x".repeat(MAX_EVENT_BYTES - empty.canonical_body.len());
        match &mut body {
            EventBody::WorkDispatched { prompt, .. } => *prompt = text,
            EventBody::WorkProgress { summary, .. } => *summary = text,
            EventBody::WorkFinished { result_text, .. } => *result_text = text,
            _ => unreachable!(),
        }
        let signed = origin.append_event(&net.a, &net.network, "c", body, vec![], 4 + index as i64)?;
        assert_eq!(signed.canonical_body.len(), MAX_EVENT_BYTES);
        prefix.push(signed);
        if index == 0 {
            super::super::scheduler::admit_from(&origin.connection, &net.network, "c",
                &crate::scheduling::task_trigger("maximal-work"), TriggerKind::AgentTask, "maximal-work")?;
            next_cursor = origin.conversation_works_page(&net.network, "c", None, 1)?.next_cursor;
        }
    }
    assert!(next_cursor.is_some(), "measure a real continuation as well as the large record");
    let mut work = serde_json::to_value(origin.work(&net.network, "maximal-work")?.context("complete work")?)?;
    let job = origin.scheduler_job("c", &crate::scheduling::task_trigger("maximal-work"))?.context("real task coordination job")?;
    work["coordination_job"] = json!({"conversation_id":job.conversation_id,"trigger":job.trigger,
        "scope":job.session_scope(),"state":job.state.name(),"sequence":job.sequence,"reply_message_id":job.message_id});
    let response = json!({"conversation_id":"c","section":"works",
        "history_authority":origin.conversation_history_authority(&net.network, "c")?,
        "items":[work],"next_cursor":next_cursor,"complete":false});
    let response_bytes = serde_json::to_vec(&response)?.len();
    let mut raw_response = response.clone();
    raw_response["items"][0].as_object_mut().context("work object")?.remove("coordination_job");
    let raw_response_bytes = serde_json::to_vec(&raw_response)?.len();
    let rpc_bytes = serde_json::to_vec(&json!({"id":"r".repeat(128),"result":raw_response}))?.len();
    eprintln!("maximal signed work: history_payload_bytes={response_bytes} raw_history_payload_bytes={raw_response_bytes} rpc_envelope_bytes={rpc_bytes} history_cap={HISTORY_RESPONSE_BYTES}");
    assert!(response_bytes > 3 * 1024 * 1024 - 64 * 1024, "regresses the overly conservative reserve");
    assert!(rpc_bytes < 4 * 1024 * 1024);
    let client_identity = vibeface_secure::Identity::generate()?;
    let server_identity = vibeface_secure::Identity::generate()?;
    let mut client = vibeface_secure::Handshake::signed(true, &client_identity, Some(server_identity.device_id()), b"history-byte-proof")?;
    let mut server = vibeface_secure::Handshake::signed(false, &server_identity, Some(client_identity.device_id()), b"history-byte-proof")?;
    assert!(server.read(&client.write(&[])?)?.is_empty());
    assert!(client.read(&server.write(&[])?)?.is_empty());
    assert!(server.read(&client.write(&[])?)?.is_empty());
    let mut sender = server.finish()?;
    let mut receiver = client.finish()?;
    let rpc_payload = serde_json::to_vec(&json!({"id":"r".repeat(128),"result":raw_response}))?;
    let records = sender.seal_message(&rpc_payload)?;
    assert!(records.iter().all(|record| record.len() <= 65_535));
    assert!(records.iter().map(Vec::len).sum::<usize>() < 4 * 1024 * 1024);
    let mut recovered = None;
    for record in &records {
        if let Some(payload) = receiver.open_record(record)? { assert!(recovered.replace(payload).is_none()); }
    }
    assert_eq!(recovered.context("reassembled maximal work RPC")?, rpc_payload);
    // Keep the legal record accessible; the actual API limit must accommodate this signed fixture.
    let page = origin.conversation_works_page(&net.network, "c", None, 1)?;
    assert_eq!(page.items.len(), 1);
    assert!(serde_json::to_vec(&page)?.len() < HISTORY_RESPONSE_BYTES);
    assert_eq!(serde_json::to_value(&page)?, raw_response);
    let older = origin.conversation_works_page(&net.network, "c", page.next_cursor.as_deref(), 1)?;
    assert_eq!(older.items.iter().map(|work| work.work_id.as_str()).collect::<Vec<_>>(), ["older-work"]);
    let observations = origin.conversation_scheduler_page(&net.network, "c", &net.a.device_id()?,
        vibeface_protocol::history::HistorySection::ScopeJobs, None, 100, |_, job| Ok(serde_json::to_value(job)?))?;
    assert!(observations.items.iter().any(|job| job["trigger"] == "task:maximal-work" && job["reply_to"] == "maximal-work"),
        "the non-null coordination observation remains available in its own section");
    assert_eq!(serde_json::to_value(&page.items[0])?["prompt"], response["items"][0]["prompt"]);
    assert_eq!(serde_json::to_value(&page.items[0])?["summary"], response["items"][0]["summary"]);
    assert_eq!(serde_json::to_value(&page.items[0])?["result_text"], response["items"][0]["result_text"]);
    Ok(())
}
