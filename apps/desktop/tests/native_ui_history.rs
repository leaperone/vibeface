#[allow(dead_code)]
#[path = "../src/views/history.rs"]
mod history;

use anyhow::Result;
use history::{ApplyResult, HistoryScope, HistoryState};
use serde_json::{Value, json};
use vibeface_protocol::history::HistorySection as S;

fn state() -> HistoryState {
    HistoryState::new(HistoryScope {
        network_id: "network".into(),
        client_id: "client".into(),
        conversation_id: "conversation".into(),
        generation: 3,
    })
}
fn snapshot(authority: &str, section: S, rows: Value, cursor: Option<&str>) -> Value {
    let mut value = json!({"conversation":{"conversation_id":"conversation","owner_epoch":1},"history_authority":authority,"history":{}});
    let section_name = serde_json::to_value(section)
        .unwrap()
        .as_str()
        .unwrap()
        .to_owned();
    value["history"][&section_name] = json!({"complete":cursor.is_none(),"next_cursor":cursor});
    let mut target = &mut value;
    let path = section.snapshot_path();
    for key in &path[..path.len() - 1] {
        if !target.get(*key).is_some_and(Value::is_object) {
            target[*key] = json!({});
        }
        target = target.get_mut(*key).unwrap();
    }
    target[path[path.len() - 1]] = rows;
    if section == S::MemorySyncConflicts {
        value["bot_settings"]["memory_sync_conflicts"]["has_more"] = json!(cursor.is_some());
        value["bot_settings"]["memory_sync_conflicts"]["extra"] = json!("preserved");
    }
    value
}
fn page(authority: &str, section: S, rows: Value, cursor: Option<&str>) -> Value {
    json!({"conversation_id":"conversation","history_authority":authority,"section":section,"items":rows,"next_cursor":cursor,"complete":cursor.is_none()})
}
fn rows(state: &HistoryState, section: S) -> Value {
    Value::Array(
        state
            .section(section)
            .rows
            .iter()
            .map(|row| row.value.clone())
            .collect(),
    )
}
fn load(state: &mut HistoryState, value: Value) -> Result<ApplyResult> {
    let ticket = state.begin_snapshot();
    state.apply_snapshot(&ticket, value)
}

#[test]
fn history_refresh_keeps_older_rows_and_late_pages_keep_newer_push_content() -> Result<()> {
    let mut state = state();
    load(
        &mut state,
        snapshot(
            "a",
            S::Messages,
            json!([{"message_id":"m3","rank":"03","text":"latest"}]),
            Some("older3"),
        ),
    )?;
    let older = state.begin_page(S::Messages)?;
    state.apply_push(json!({"conversation_id":"conversation","history_authority":"a","messages":[{"message_id":"m3","rank":"03","text":"pushed","attachments":[{"hash":"original-complete","metadata":{"all":"retained"}}]}]}))?;
    state.apply_page(&older, page("a", S::Messages, json!([{"message_id":"m1","rank":"01","text":"old"},{"message_id":"m3","rank":"03","text":"stale-page"}]), None))?;
    assert_eq!(
        rows(&state, S::Messages),
        json!([
            {"message_id":"m1","rank":"01","text":"old"},
            {"message_id":"m3","rank":"03","text":"pushed","attachments":[{"hash":"original-complete","metadata":{"all":"retained"}}]}
        ])
    );
    load(
        &mut state,
        snapshot(
            "a",
            S::Messages,
            json!([{"message_id":"m4","rank":"04","text":"new arrival"}]),
            Some("older4"),
        ),
    )?;
    assert_eq!(
        state
            .section(S::Messages)
            .rows
            .iter()
            .map(|row| row.value["message_id"].as_str().unwrap())
            .collect::<Vec<_>>(),
        vec!["m1", "m3", "m4"]
    );
    assert!(
        state
            .section(S::Messages)
            .coverage
            .as_ref()
            .unwrap()
            .complete,
        "refresh does not reset an already traversed cursor"
    );
    Ok(())
}

#[test]
fn authority_changes_clear_invalid_rows_and_reject_late_scope_results() -> Result<()> {
    let mut state = state();
    load(
        &mut state,
        snapshot("a", S::Works, json!([{"work_id":"old"}]), Some("cursor")),
    )?;
    let old_page = state.begin_page(S::Works)?;
    let old_snapshot = state.begin_snapshot();
    assert_eq!(state.apply_push(json!({"conversation_id":"conversation","history_authority":"b","messages":[{"message_id":"new","text":"new authority"}]}))?, ApplyResult::AuthorityReset);
    assert_eq!(rows(&state, S::Works), json!([]));
    assert_eq!(
        state.apply_page(
            &old_page,
            page("a", S::Works, json!([{"work_id":"invalid"}]), None)
        )?,
        ApplyResult::Stale
    );
    assert_eq!(
        state.apply_snapshot(
            &old_snapshot,
            snapshot("a", S::Works, json!([{"work_id":"invalid"}]), None)
        )?,
        ApplyResult::Stale
    );
    assert_eq!(state.materialize()["history_authority"], json!("b"));
    let foreign = HistoryState::new(HistoryScope {
        generation: 4,
        ..state.scope().clone()
    });
    let foreign_ticket = foreign.begin_snapshot();
    assert_eq!(
        state.apply_snapshot(&foreign_ticket, snapshot("b", S::Works, json!([]), None))?,
        ApplyResult::Stale
    );
    state.invalidate();
    assert!(state.materialize()["history_authority"].is_null());
    assert_eq!(rows(&state, S::Messages), json!([]));
    Ok(())
}

#[test]
fn partial_push_retains_absent_lists_and_explicit_empty_clears_only_present_section() -> Result<()>
{
    let mut state = state();
    load(
        &mut state,
        snapshot(
            "a",
            S::Works,
            json!([{"work_id":"w2","title":"kept"}]),
            Some("older"),
        ),
    )?;
    load(
        &mut state,
        snapshot(
            "a",
            S::Questions,
            json!([{"question_id":"q1","text":"keep question"}]),
            None,
        ),
    )?;
    state.apply_push(json!({"conversation_id":"conversation","history_authority":"a","messages":[{"message_id":"m1"}]}))?;
    assert_eq!(
        rows(&state, S::Works),
        json!([{"work_id":"w2","title":"kept"}])
    );
    assert_eq!(
        rows(&state, S::Questions),
        json!([{"question_id":"q1","text":"keep question"}])
    );
    state
        .apply_push(json!({"conversation_id":"conversation","history_authority":"a","works":[]}))?;
    assert_eq!(rows(&state, S::Works), json!([]));
    assert_eq!(
        rows(&state, S::Questions),
        json!([{"question_id":"q1","text":"keep question"}])
    );
    assert_eq!(state.materialize()["works"], json!([]));
    Ok(())
}

#[test]
fn empty_continuation_advances_and_unranked_works_keep_server_order() -> Result<()> {
    let mut state = state();
    load(
        &mut state,
        snapshot(
            "a",
            S::Works,
            json!([{"work_id":"w9"},{"work_id":"w5"}]),
            Some("c1"),
        ),
    )?;
    let first = state.begin_page(S::Works)?;
    assert_eq!(
        state.apply_page(&first, page("a", S::Works, json!([]), Some("c2")))?,
        ApplyResult::Applied
    );
    assert_eq!(
        state.begin_page(S::Works)?.request().cursor.as_deref(),
        Some("c2")
    );
    assert_eq!(
        state.apply_page(
            &first,
            page(
                "a",
                S::Works,
                json!([{"work_id":"duplicate-old-page"}]),
                None
            )
        )?,
        ApplyResult::Stale
    );
    let second = state.begin_page(S::Works)?;
    state.apply_page(
        &second,
        page(
            "a",
            S::Works,
            json!([{"work_id":"w8"},{"work_id":"w1"}]),
            None,
        ),
    )?;
    assert_eq!(
        rows(&state, S::Works),
        json!([{"work_id":"w8"},{"work_id":"w1"},{"work_id":"w9"},{"work_id":"w5"}])
    );
    assert!(state.begin_page(S::Works).is_err());
    Ok(())
}

#[test]
fn snapshot_started_before_push_cannot_replace_newer_rows() -> Result<()> {
    let mut state = state();
    load(
        &mut state,
        snapshot(
            "a",
            S::Messages,
            json!([{"message_id":"m","text":"initial"}]),
            Some("c"),
        ),
    )?;
    let snapshot_ticket = state.begin_snapshot();
    state.apply_push(json!({"conversation_id":"conversation","history_authority":"a","messages":[{"message_id":"m","text":"pushed"}]}))?;
    state.apply_snapshot(
        &snapshot_ticket,
        snapshot(
            "a",
            S::Messages,
            json!([{"message_id":"m","text":"late snapshot"}]),
            None,
        ),
    )?;
    assert_eq!(
        rows(&state, S::Messages),
        json!([{"message_id":"m","text":"pushed"}])
    );
    Ok(())
}

#[test]
fn all_protocol_sections_parse_without_dropping_content_and_named_lists_sort() -> Result<()> {
    let records = [
        (S::Messages, json!({"message_id":"m","text":"full"})),
        (S::Works, json!({"work_id":"w","result_text":"full"})),
        (
            S::Changes,
            json!({"change_id":"c","change":{"kind":"memory","after":"full"}}),
        ),
        (
            S::Questions,
            json!({"question_id":"q","choices":[{"label":"full"}]}),
        ),
        (
            S::Sessions,
            json!({"node":"node","session_id":"session","cwd":"full"}),
        ),
        (
            S::Jobs,
            json!({"trigger":"job","failure":{"message":"full"}}),
        ),
        (
            S::ScopeJobs,
            json!({"trigger":"scope-job","scope":{"kind":"task","work_id":"full"}}),
        ),
        (S::OwnerJobs, json!({"trigger":"owner-job","extra":"full"})),
        (
            S::OwnerScopeJobs,
            json!({"trigger":"owner-scope-job","extra":"full"}),
        ),
        (
            S::OwnerMemoryConflicts,
            json!({"history_id":"owner-conflict","memory_id":"memory","detail":"full"}),
        ),
        (
            S::Checks,
            json!({"check_id":"check","process_groups":[1,2]}),
        ),
        (
            S::Deliveries,
            json!({"work_id":"delivery","state":"waiting","prompt":"full"}),
        ),
        (S::Unanswered, json!("unanswered-message")),
        (
            S::Skills,
            json!({"skill_id":"skill","name":"Native","description":"full"}),
        ),
        (
            S::Schedules,
            json!({"schedule_id":"schedule","name":"Native","prompt":"full"}),
        ),
        (
            S::Memory,
            json!({"memory_id":"memory","text":"full","metadata":{"epoch":1}}),
        ),
        (
            S::MemoryConflicts,
            json!({"history_id":"local-conflict","memory_id":"memory","detail":"full"}),
        ),
        (
            S::MemorySyncConflicts,
            json!({"memory_id":"memory","losing_revision":"revision","winning_event_hash":"full"}),
        ),
        (
            S::Overwritten,
            json!({"change_id":"change","field":"model","provider":"codex","value":{"full":true}}),
        ),
    ];
    assert_eq!(records.len(), S::ALL.len());
    let mut state = state();
    for (section, record) in records {
        let raw = snapshot("a", section, json!([record.clone()]), None);
        load(&mut state, raw)?;
        assert_eq!(rows(&state, section), json!([record]));
    }
    assert_eq!(
        state.materialize()["bot_settings"]["memory_sync_conflicts"]["items"][0]["winning_event_hash"],
        "full"
    );
    load(
        &mut state,
        snapshot(
            "a",
            S::Skills,
            json!([
                {"skill_id":"z","name":"Alpha"},{"skill_id":"b","name":"alpha"},{"skill_id":"a","name":"Beta"}
            ]),
            None,
        ),
    )?;
    assert_eq!(
        state
            .section(S::Skills)
            .rows
            .iter()
            .map(|row| row.value["skill_id"].as_str().unwrap())
            .collect::<Vec<_>>(),
        vec!["b", "z", "a"]
    );
    Ok(())
}

#[test]
fn malformed_or_cross_scope_rows_fail_without_mutating_history() -> Result<()> {
    let mut state = state();
    load(
        &mut state,
        snapshot("a", S::Works, json!([{"work_id":"kept"}]), Some("c")),
    )?;
    let page_ticket = state.begin_page(S::Works)?;
    assert!(
        state
            .apply_page(
                &page_ticket,
                page(
                    "a",
                    S::Works,
                    json!([{"work_id":"x","conversation_id":"other"}]),
                    None
                )
            )
            .is_err()
    );
    assert!(
        load(
            &mut state,
            snapshot("a", S::Works, json!([{"title":"missing id"}]), None)
        )
        .is_err()
    );
    assert!(
        load(
            &mut state,
            snapshot(
                "a",
                S::Works,
                json!([{"work_id":"dup"},{"work_id":"dup"}]),
                None
            )
        )
        .is_err()
    );
    assert_eq!(rows(&state, S::Works), json!([{"work_id":"kept"}]));
    Ok(())
}

#[test]
fn delivery_stop_and_dispatch_have_distinct_stable_ids_and_late_snapshot_cannot_resurrect_clear()
-> Result<()> {
    let mut state = state();
    load(
        &mut state,
        snapshot(
            "a",
            S::Deliveries,
            json!([
                {"work_id":"w","state":"waiting","prompt":"dispatch"},
                {"work_id":"w","state":"stop_waiting","target_node":"node"}
            ]),
            None,
        ),
    )?;
    assert_eq!(state.section(S::Deliveries).rows.len(), 2);
    let stale = state.begin_snapshot();
    state.apply_push(
        json!({"conversation_id":"conversation","history_authority":"a","deliveries":[]}),
    )?;
    state.apply_snapshot(
        &stale,
        snapshot(
            "a",
            S::Deliveries,
            json!([{"work_id":"w","state":"waiting"}]),
            None,
        ),
    )?;
    assert_eq!(rows(&state, S::Deliveries), json!([]));
    Ok(())
}

#[test]
fn complete_refresh_removal_cannot_be_undone_by_an_older_snapshot() -> Result<()> {
    let mut state = state();
    load(
        &mut state,
        snapshot(
            "a",
            S::Questions,
            json!([{"question_id":"removed"},{"question_id":"kept"}]),
            None,
        ),
    )?;
    let older = state.begin_snapshot();
    load(
        &mut state,
        snapshot("a", S::Questions, json!([{"question_id":"kept"}]), None),
    )?;
    state.apply_snapshot(
        &older,
        snapshot(
            "a",
            S::Questions,
            json!([{"question_id":"removed"},{"question_id":"kept"}]),
            None,
        ),
    )?;
    assert_eq!(rows(&state, S::Questions), json!([{"question_id":"kept"}]));
    Ok(())
}

#[test]
fn omitted_section_with_coverage_remains_retrievable() -> Result<()> {
    let mut state = state();
    load(
        &mut state,
        json!({
            "conversation":{"conversation_id":"conversation"},"history_authority":"a",
            "history":{"memory":{"next_cursor":"start-memory","complete":false}}
        }),
    )?;
    assert_eq!(
        state.begin_page(S::Memory)?.request().cursor.as_deref(),
        Some("start-memory")
    );
    assert_eq!(state.materialize()["bot_settings"]["memory"], json!([]));
    Ok(())
}

#[test]
fn overwritten_restore_uses_matching_original_after_and_keeps_full_change_bodies() -> Result<()> {
    let original: history::BotChangeRecord = serde_json::from_value(json!({
        "change_id":"change","conversation_id":"conversation","cause":null,"event_hash":"hash","created_at_ms":1,
        "change":{"kind":"profile","name":{"before":"before","after":"original after"},"model":{"provider":"codex","before":"old-model","after":"original-model"},"skills":{"before":[],"after":null}}
    }))?;
    let overwritten: history::OverwrittenRecord = serde_json::from_value(
        json!({"change_id":"change","field":"name","by":"other","value":"current winner"}),
    )?;
    assert_eq!(
        overwritten.original_after(&original)?,
        json!("original after")
    );
    let wrong: history::OverwrittenRecord = serde_json::from_value(
        json!({"change_id":"missing","field":"name","by":"other","value":"current winner"}),
    )?;
    assert!(wrong.original_after(&original).is_err());
    let wrong_provider: history::OverwrittenRecord = serde_json::from_value(
        json!({"change_id":"change","field":"model","provider":"claude","by":"other","value":"current"}),
    )?;
    assert!(wrong_provider.original_after(&original).is_err());
    let skills: history::OverwrittenRecord = serde_json::from_value(
        json!({"change_id":"change","field":"skills","by":"other","value":["current"]}),
    )?;
    assert_eq!(skills.original_after(&original)?, Value::Null);
    let skill: history::BotChange = serde_json::from_value(
        json!({"kind":"skill","skill_id":"id","before":null,"after":{"name":"Skill","description":"Full description","body":"Full workflow content","extra":"retained"}}),
    )?;
    let history::BotChange::Skill {
        after: Some(body), ..
    } = skill
    else {
        panic!("typed skill change expected");
    };
    assert_eq!(body.body, "Full workflow content");
    assert_eq!(body.fields.get("extra"), Some(&json!("retained")));
    Ok(())
}

#[test]
fn reconnect_starts_a_new_history_window_and_rejects_old_page_tickets() -> Result<()> {
    let mut old = state();
    load(
        &mut old,
        snapshot(
            "a",
            S::Works,
            json!([{"work_id":"old-running","state":"running"}]),
            Some("old-cursor"),
        ),
    )?;
    let ticket = old.begin_page(S::Works)?;
    let mut fresh = HistoryState::new(HistoryScope {
        generation: 4,
        ..old.scope().clone()
    });
    load(
        &mut fresh,
        snapshot(
            "a",
            S::Works,
            json!([{"work_id":"latest","state":"completed"}]),
            None,
        ),
    )?;
    assert_eq!(
        rows(&fresh, S::Works),
        json!([{"work_id":"latest","state":"completed"}])
    );
    assert_eq!(
        fresh.apply_page(
            &ticket,
            page(
                "a",
                S::Works,
                json!([{"work_id":"old-running","state":"running"}]),
                None
            )
        )?,
        ApplyResult::Stale
    );
    Ok(())
}

#[test]
fn restoration_never_uses_the_winning_value_or_another_provider() -> Result<()> {
    let original: history::BotChangeRecord = serde_json::from_value(
        json!({"change_id":"mine","conversation_id":"conversation","cause":null,"event_hash":"event","created_at_ms":1,"change":{"kind":"profile","model":{"provider":"codex","before":"old","after":"my-model"}}}),
    )?;
    let record: history::OverwrittenRecord = serde_json::from_value(
        json!({"change_id":"mine","by":"remote","field":"model","provider":"codex","value":"their-model"}),
    )?;
    match record.restoration(&original)? {
        history::ProfileRestoration::Instruction(text) => {
            assert_eq!(text, "请仅把 Codex 模型改回「my-model」，保留其他配置。")
        }
        _ => panic!("wrong restoration type"),
    }
    let foreign: history::OverwrittenRecord = serde_json::from_value(
        json!({"change_id":"mine","by":"remote","field":"model","provider":"claude","value":"their-model"}),
    )?;
    assert!(foreign.restoration(&original).is_err());
    Ok(())
}

#[test]
fn empty_owner_coverage_never_fabricates_an_owner_observation() -> Result<()> {
    let mut history = state();
    let mut value = snapshot("a", S::Jobs, json!([]), None);
    value["scheduler"]["remote"] = Value::Null;
    for section in [S::OwnerJobs, S::OwnerScopeJobs, S::OwnerMemoryConflicts] {
        let name = serde_json::to_value(section)?.as_str().unwrap().to_owned();
        value["history"][name] = json!({"next_cursor":null,"complete":true});
    }
    load(&mut history, value)?;
    assert_eq!(history.materialize()["scheduler"]["remote"], Value::Null);
    assert_eq!(
        history.materialize()["history"]["owner_jobs"],
        json!({"next_cursor":null,"complete":true})
    );
    Ok(())
}
