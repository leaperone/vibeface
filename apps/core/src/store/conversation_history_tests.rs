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
