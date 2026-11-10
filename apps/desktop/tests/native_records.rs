#[path = "../src/views/records.rs"]
#[allow(dead_code)]
mod records;
use records::*;
use serde_json::Value;
use std::collections::BTreeMap;

// Literal wire fixtures follow Core's WorkProjection (apps/core/src/model.rs),
// runtime/chat_controls.rs::work_detail and runtime/workspace.rs::workspace_history.
// They are not serialized from the native DTOs under test. Core omits attestation on
// ordinary Node records and includes it on human inputs; both cases must parse.
const WORK: &str = r###"{
  "work": {
    "work_id":"work-1","conversation_id":"chat-a","target_node":"node-a",
    "trigger":"input:message-1","dispatcher":"node-b","kind":"codex",
    "cwd":"/fixture/project","title":"检查附件","prompt":"检查输入，不调用真实 Provider。",
    "call_ref":"call-1","model":null,"resolved":{"provider":"codex","model":null},
    "continues":null,"parent":null,"thread":null,"phase":"finishing",
    "summary":"已读取附件","thread_id":"thread-1","turn_id":"turn-1",
    "outcome":"completed","result_text":"## 完成\n\n受控结果。","failure_reason":null,
    "failure":null,"state":"finished","target":{"kind":"channel_task","channel_id":"channel-a","task_id":"task-a","bot_id":"bot-a"},
    "source_events":["event-dispatched","event-finished"]
  },
  "coverage":{"source":"shared_history","complete":false}
}"###;
const HISTORY: &str = r###"{
  "events":[
    {"event_hash":"event-input","created_at_ms":1,"scope_id":"chat-a","kind":"input_posted",
      "speaker":{"kind":"human","device_id":"client-a"},
      "body":{"type":"input_posted","message_id":"message-1","target":{"kind":"channel_task","channel_id":"channel-a","task_id":"task-a","bot_id":"bot-a"},"intent":"continue","sequence":1,"expected_revision":2,"text":"读取此文件",
        "attachments":[{"hash":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","size":6,"name":"输入.txt"}],
        "attestation":{"device_id":"client-a","signature":"fixture-signature"}}},
    {"event_hash":"event-reply","created_at_ms":2,"scope_id":"chat-a","kind":"message_posted",
      "speaker":{"kind":"bot","bot_id":"bot-a"},
      "body":{"type":"message_posted","message_id":"reply-1","role":"assistant","text":"## 文件已读取","in_reply_to":"message-1","status":"completed",
        "attachments":[{"hash":"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb","size":3,"name":"output.txt"}]}},
    {"event_hash":"event-post","created_at_ms":3,"scope_id":"workspace","kind":"workspace_commanded",
      "speaker":{"kind":"human","device_id":"client-a"},
      "body":{"type":"workspace_commanded","command_id":"post-1","command":{"kind":"post_channel_message","channel_id":"channel-a","message_id":"posted-1","text":"保留频道记录","recipient_bot":"bot-a","task_id":"task-a","reply_to":"message-1",
        "attachments":[{"hash":"cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc","size":4,"name":"channel.txt"}]}}},
    {"event_hash":"event-dispatched","created_at_ms":4,"scope_id":"chat-a","kind":"work_dispatched",
      "speaker":{"kind":"node","device_id":"node-a"},
      "body":{"type":"work_dispatched","work_id":"work-1","trigger":"input:message-1","dispatcher":"node-b","call_ref":"call-1","kind":"codex","resolved":{"provider":"codex","model":null},"cwd":"/fixture/project","title":"检查附件","prompt":"检查输入，不调用真实 Provider。"}},
    {"event_hash":"event-progress","created_at_ms":5,"scope_id":"chat-a","kind":"work_progress",
      "speaker":{"kind":"node","device_id":"node-a"},
      "body":{"type":"work_progress","work_id":"work-1","phase":"running","summary":"读取中","thread_id":"thread-1","turn_id":"turn-1"}},
    {"event_hash":"event-finished","created_at_ms":6,"scope_id":"chat-a","kind":"work_finished",
      "speaker":{"kind":"node","device_id":"node-a"},
      "body":{"type":"work_finished","work_id":"work-1","outcome":"completed","result_text":"受控结果。"}}
  ],
  "coverage":{"limit":100,"truncated":true,"scope":"channel"}
}"###;

fn bindings() -> HistoryBindings {
    HistoryBindings {
        bot_conversations:BTreeMap::from([("bot-a".into(),"chat-a".into()),("bot-b".into(),"chat-b".into())]),
        task_channels:BTreeMap::from([("task-a".into(),Some("channel-a".into())),("task-b".into(),Some("channel-b".into()))]),
        task_owners:BTreeMap::from([("task-a".into(),"bot-a".into()),("task-b".into(),"bot-b".into())]),
        work_conversations:BTreeMap::from([("work-1".into(),"chat-a".into())]),
        channel_coordinators:BTreeMap::from([("channel-a".into(),"bot-a".into()),("channel-b".into(),"bot-b".into())]),
    }
}
fn history(value:Value)->anyhow::Result<WorkspaceHistory>{parse_workspace_history(value,"network-a",Some("channel-a"),None,&bindings())}

#[test]
fn literal_core_work_retains_terminal_result_runtime_and_incomplete_coverage(){
    let detail=parse_work_detail(serde_json::from_str(WORK).unwrap(),"network-a","chat-a","work-1").unwrap();
    assert_eq!(detail.network_id,"network-a");
    assert_eq!(detail.work.result_text,"## 完成\n\n受控结果。");
    assert_eq!(detail.work.state,WorkState::Finished);
    assert_eq!(detail.work.outcome,Some(WorkOutcome::Completed));
    assert_eq!(detail.work.thread_id.as_deref(),Some("thread-1"));
    assert_eq!(detail.work.turn_id.as_deref(),Some("turn-1"));
    assert_eq!(detail.work.source_events,["event-dispatched","event-finished"]);
    assert_eq!(detail.coverage.source,"shared_history");
    assert!(!detail.coverage.complete);
}

#[test]
fn work_rejects_wrong_conversation_work_malformed_state_and_false_complete_history(){
    let value:Value=serde_json::from_str(WORK).unwrap();
    assert!(parse_work_detail(value.clone(),"network-a","chat-b","work-1").is_err());
    assert!(parse_work_detail(value.clone(),"network-a","chat-a","work-2").is_err());
    let mut wrong=value.clone();wrong["work"]["state"]="running".into();
    assert!(parse_work_detail(wrong,"network-a","chat-a","work-1").is_err());
    let mut wrong=value.clone();wrong["coverage"]["complete"]=true.into();
    assert!(parse_work_detail(wrong,"network-a","chat-a","work-1").is_err());
    let mut wrong=value.clone();wrong["work"].as_object_mut().unwrap().remove("thread_id");
    assert!(parse_work_detail(wrong,"network-a","chat-a","work-1").is_err());
    let mut wrong=value;wrong["work"].as_object_mut().unwrap().remove("resolved");
    assert!(parse_work_detail(wrong,"network-a","chat-a","work-1").is_err());
}

#[test]
fn native_history_retains_verified_speakers_replies_task_targets_and_original_attachment_scopes(){
    let data=history(serde_json::from_str(HISTORY).unwrap()).unwrap();
    assert_eq!(data.events.len(),6);
    assert_eq!(data.network_id,"network-a");
    assert_eq!(data.channel_id.as_deref(),Some("channel-a"));
    assert!(data.coverage.truncated);
    assert_eq!(data.events[0].attachment_conversation.as_deref(),Some("chat-a"));
    assert_eq!(data.events[1].attachment_conversation.as_deref(),Some("chat-a"));
    // Core stores channel command blobs in the workspace event scope. The receiver Bot's
    // conversation is not their read scope, even when the command names that Bot and task.
    assert_eq!(data.events[2].attachment_conversation.as_deref(),Some("workspace"));
    assert_eq!(data.events[2].body.attachments()[0].name,"channel.txt");
    assert!(data.events[0].search_text.contains("输入.txt"));
    assert!(data.events[1].search_text.contains("文件已读取"));
    assert!(matches!(&data.events[1].speaker,vibeface_protocol::workspace::WorkspaceSpeaker::Bot{bot_id} if bot_id=="bot-a"));
    match &data.events[1].body{HistoryBody::MessagePosted{in_reply_to,status,..}=>{assert_eq!(in_reply_to.as_deref(),Some("message-1"));assert!(matches!(status,Some(ReplyStatus::Completed)));},_=>panic!("expected Core message record")}
}

#[test]
fn history_rejects_cross_channel_task_conversation_and_work_identity(){
    let value:Value=serde_json::from_str(HISTORY).unwrap();
    let mut wrong=value.clone();wrong["events"][0]["body"]["target"]["channel_id"]="channel-b".into();assert!(history(wrong).is_err());
    let mut wrong=value.clone();wrong["events"][0]["body"]["target"]["task_id"]="task-b".into();assert!(history(wrong).is_err());
    let mut wrong=value.clone();wrong["events"][0]["scope_id"]="chat-b".into();assert!(history(wrong).is_err());
    let mut wrong=value.clone();wrong["events"][1]["scope_id"]="chat-b".into();assert!(history(wrong).is_err());
    let mut wrong=value.clone();wrong["events"][1]["speaker"]["bot_id"]="bot-b".into();assert!(history(wrong).is_err());
    let mut wrong=value.clone();wrong["events"][3]["scope_id"]="chat-b".into();assert!(history(wrong).is_err());
    let mut wrong=value;wrong["events"][2]["body"]["command"]["channel_id"]="channel-b".into();assert!(history(wrong).is_err());
}

#[test]
fn malformed_history_is_an_error_instead_of_a_synced_empty_page(){
    assert!(history(serde_json::json!({"events":[],"coverage":{"limit":100,"scope":"channel"}})).is_err());
    let value:Value=serde_json::from_str(HISTORY).unwrap();
    let mut wrong=value.clone();wrong["events"][0]["kind"]="message_posted".into();assert!(history(wrong).is_err());
    let mut wrong=value.clone();wrong["events"][0]["speaker"]=serde_json::json!({"kind":"bot","device_id":"node-a"});assert!(history(wrong).is_err());
    let mut wrong=value.clone();wrong["events"][3]["body"].as_object_mut().unwrap().remove("resolved");assert!(history(wrong).is_err());
    let mut wrong=value.clone();wrong["coverage"]["scope"]="task".into();assert!(history(wrong).is_err());
    let mut wrong=value;wrong["coverage"]["limit"]=1.into();assert!(history(wrong).is_err());
}

#[test]
fn attachment_metadata_rejects_missing_hash_path_names_oversize_and_wrong_shape(){
    let value:Value=serde_json::from_str(HISTORY).unwrap();
    for (key,bad) in [("hash",Value::String("bad".into())),("name",Value::String("../secret".into())),("size",Value::from(67108865u64))]{
        let mut wrong=value.clone();wrong["events"][1]["body"]["attachments"][0][key]=bad;assert!(history(wrong).is_err());
    }
    let mut wrong=value;wrong["events"][1]["body"]["attachments"]=Value::String("files".into());assert!(history(wrong).is_err());
}

#[test]
fn native_artifact_pages_preserve_utf8_byte_offsets_and_exact_hash_size_identity(){
    let data=history(serde_json::from_str(HISTORY).unwrap()).unwrap();
    let file=&data.events[0].body.attachments()[0];
    let page:Value=serde_json::from_str(r#"{"blob_hash":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","text":"你","offset":0,"next_offset":3,"size":6}"#).unwrap();
    let preview=parse_artifact_page(page.clone(),"network-a","chat-a",file,0).unwrap();
    assert_eq!(preview.conversation_id,"chat-a");assert_eq!(preview.text,"你");assert_eq!(preview.next_offset,Some(3));
    let mut wrong=page.clone();wrong["next_offset"]=1.into();assert!(parse_artifact_page(wrong,"network-a","chat-a",file,0).is_err());
    let mut wrong=page.clone();wrong["blob_hash"]="bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb".into();assert!(parse_artifact_page(wrong,"network-a","chat-a",file,0).is_err());
    assert!(parse_artifact_page(page,"network-a","chat-a",file,3).is_err());
}

#[test]
fn unknown_work_outcomes_remain_unknown_and_never_become_a_completed_result(){
    let mut value:Value=serde_json::from_str(WORK).unwrap();value["work"]["state"]="unverified".into();value["work"]["outcome"]="unverified".into();value["work"]["result_text"]="".into();
    let detail=parse_work_detail(value,"network-a","chat-a","work-1").unwrap();
    assert_eq!(detail.work.state,WorkState::Unverified);assert_eq!(detail.work.outcome,Some(WorkOutcome::Unverified));assert_eq!(detail.work.result_text,"");
}

#[test]
fn empty_history_is_accepted_only_with_explicit_matching_coverage(){
    let data=history(serde_json::from_str(r#"{"events":[],"coverage":{"limit":100,"truncated":false,"scope":"channel"}}"#).unwrap()).unwrap();
    assert_eq!(data.events.len(),0);assert!(!data.coverage.truncated);
    assert!(parse_workspace_history(serde_json::from_str(HISTORY).unwrap(),"network-a",Some("channel-b"),Some("task-a"),&bindings()).is_err());
}

#[test]
fn rendered_record_actions_reject_stale_connection_or_network_even_when_record_ids_match(){
    let rendered=RecordScope{generation:7,network_id:Some("network-a".into())};
    assert!(rendered.accepts(7,Some("network-a")));
    assert!(!rendered.accepts(8,Some("network-a")));
    assert!(!rendered.accepts(7,Some("network-b")));
    assert!(!rendered.accepts(7,None));
}
