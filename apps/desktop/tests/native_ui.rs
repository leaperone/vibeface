#[path="../src/ui.rs"]
#[allow(dead_code)]
mod ui;
use serde_json::json;
use vibeface_protocol::workspace::{Attachment,InputIntent,InputTarget};
use ui::{ComposerDraft,Conversation,decode_list};

#[test]
fn native_input_rejects_empty_and_keeps_attachment_only_messages(){
 let mut draft=ComposerDraft{target:InputTarget::BotMain{bot_id:"bot-a".into()},intent:InputIntent::Continue,text:" \n ".into(),attachments:vec![],expected_revision:None};
 assert_eq!(draft.validate(),Err("请输入消息或添加附件".into()));
 draft.attachments.push(Attachment{hash:"a".repeat(64),size:3,name:"note.txt".into()});
 assert_eq!(draft.validate(),Ok(()));
}
#[test]
fn native_conversation_projection_keeps_markdown_stop_triggers_and_memory_cas(){
 let wire=json!({"messages":[{"message_id":"m1","role":"assistant","text":"## 原生\n\n`code`","event_hash":"e1","attachments":[{"hash":"abc","size":12,"name":"result.txt"}]}],"scheduler":{"jobs":[{"trigger":"input-7","state":"running"}]},"bot_settings":{"memory":[{"memory_id":"memory-1","text":"本人管理设备","metadata":{"epoch":1,"revision":"r2","control_revision":3,"source_event_hash":"e9","user_controlled":true}}]}});
 let conversation:Conversation=serde_json::from_value(wire).unwrap();
 assert_eq!(conversation.messages[0].text,"## 原生\n\n`code`");
 assert_eq!(conversation.messages[0].attachments[0].name,"result.txt");
 assert_eq!(conversation.scheduler.jobs[0].trigger,"input-7");
 assert_eq!(conversation.bot_settings.memory[0].metadata.revision,"r2");
 assert!(conversation.bot_settings.memory[0].metadata.user_controlled);
}
#[test]
fn absent_scheduler_is_not_an_active_reply(){
 let conversation:Conversation=serde_json::from_value(json!({"messages":[],"scheduler":null})).unwrap();
 assert!(conversation.scheduler.jobs.is_empty());
}
#[test]
fn malformed_wire_lists_do_not_look_like_a_synced_empty_workspace(){
 assert!(decode_list::<ui::Bot>(&json!({"unexpected":[]}),"bots").is_err());
 assert!(decode_list::<ui::Bot>(&json!({"bots":[{"name":"missing identities"}]}),"bots").is_err());
 assert!(decode_list::<ui::Bot>(&json!({"bots":[]}),"bots").unwrap().is_empty());
}
#[test]
fn every_native_input_target_serializes_exact_routing_identity(){
 let targets=[InputTarget::BotMain{bot_id:"b".into()},InputTarget::Task{task_id:"t".into()},InputTarget::Work{work_id:"w".into()},InputTarget::Routine{bot_id:"b".into(),trigger:"r".into()},InputTarget::ChannelMember{channel_id:"c".into(),bot_id:"b".into()},InputTarget::ChannelTask{channel_id:"c".into(),task_id:"t".into(),bot_id:"b".into()}];
 let actual=targets.into_iter().map(|target|serde_json::to_value(target).unwrap()).collect::<Vec<_>>();
 assert_eq!(actual,vec![json!({"kind":"bot_main","bot_id":"b"}),json!({"kind":"task","task_id":"t"}),json!({"kind":"work","work_id":"w"}),json!({"kind":"routine","bot_id":"b","trigger":"r"}),json!({"kind":"channel_member","channel_id":"c","bot_id":"b"}),json!({"kind":"channel_task","channel_id":"c","task_id":"t","bot_id":"b"})]);
}
