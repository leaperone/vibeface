#![cfg(feature = "native-ui-tests")]
extern crate gpui_kit as gpui;
#[path = "../src/app.rs"]
#[allow(dead_code, unused_imports)]
mod app;
#[path = "../src/host/mod.rs"]
#[allow(dead_code, unused_imports)]
mod host;
#[path = "../src/theme.rs"]
#[allow(dead_code)]
mod theme;
#[path = "../src/ui.rs"]
#[allow(dead_code)]
mod ui;
#[path = "../src/views/mod.rs"]
#[allow(dead_code, unused_imports)]
mod views;
#[path = "../src/widgets/mod.rs"]
#[allow(dead_code)]
mod widgets;
use gpui_kit::{AnyWindowHandle, AppContext, Entity, TestAppContext, WindowOptions};
use gpui_kit::test::TestWindowExt;
use std::sync::Arc;
use tokio::sync::{mpsc,watch};
use ui::{Bot,Conversation,ConversationIdentity,Route,Snapshot};
use views::backend::{Command,Event};
use vibeface_protocol::workspace::{InputIntent,InputTarget};

struct Harness{
 window:AnyWindowHandle,
 view:Entity<app::Desktop>,
 commands:mpsc::Receiver<Command>,
 events:mpsc::Sender<Event>,
 _snapshots:watch::Sender<Arc<Snapshot>>,
 _stop:watch::Receiver<bool>,
}
fn fixture()->Snapshot{Snapshot{
 generation:7,online:true,synced:true,writable:true,can_save_locally:true,network:Some("network-fixture".into()),client_id:Some("client-fixture".into()),
 bots:vec![Bot{bot_id:"bot-a".into(),conversation_id:"chat-a".into(),name:"Alpha".into(),..Default::default()},Bot{bot_id:"bot-b".into(),conversation_id:"chat-b".into(),name:"Beta".into(),..Default::default()}],
 conversation:Arc::new(Conversation{conversation:Some(ConversationIdentity{conversation_id:"chat-a".into(),owner:Some("node-a".into()),owner_epoch:1}),..Default::default()}),..Default::default()
}}
fn open(cx:&mut TestAppContext,route:Route,snapshot:Snapshot)->Harness{
 cx.update(gpui_kit::init);
 let(tx,commands)=mpsc::channel(32);let(events,event_rx)=mpsc::channel(32);let(snapshots,snapshot_rx)=watch::channel(Arc::new(snapshot.clone()));let(stop,stop_rx)=watch::channel(false);
 let(window,view)=cx.update(|cx|gpui_kit::open_window(WindowOptions::default(),cx,move|window,cx|cx.new(|cx|app::Desktop::new(tx,stop,event_rx,snapshot_rx,window,cx))).unwrap());
 view.update(cx,|view,cx|{view.snapshot=Arc::new(snapshot);view.route=route;view.selected=Some("chat-a".into());view.attachment_scope=Some("chat-a".into());cx.notify();});
 Harness{window,view,commands,events,_snapshots:snapshots,_stop:stop_rx}
}
fn enter_message(h:&Harness,cx:&mut TestAppContext,text:&str){h.window.update(cx,|_,window,cx|h.view.update(cx,|view,cx|view.composer.update(cx,|input,cx|input.set_value(text.to_owned(),window,cx)))).unwrap();}
fn click(h:&Harness,cx:&mut TestAppContext,id:&str){let id=id.to_owned();h.window.update(cx,|_,window,cx|{window.render_frame(cx);window.click(gpui_kit::SharedString::from(id),cx);}).unwrap();cx.run_until_parked();}
fn scoped(command:Command)->(u64,Command){match command{Command::Scoped{generation,command}=>(generation,*command),_=>panic!("network action was not fenced by its rendered generation")}}

#[gpui_kit::test]
fn explicit_task_selection_routes_to_its_owner_not_the_open_bot(cx:&mut TestAppContext){
 let mut snapshot=fixture();snapshot.tasks=vec![ui::Task{task_id:"task-b".into(),owner_bot:"bot-b".into(),title:"Beta task".into(),revision:9,..Default::default()}];
 let mut h=open(cx,Route::Chat("chat-a".into()),snapshot);
 h.view.update(cx,|view,cx|{view.target_kind=1;cx.notify();});enter_message(&h,cx,"针对 Beta 的补充");click(&h,cx,"target-choice-0");click(&h,cx,"send");
 let(generation,command)=scoped(h.commands.try_recv().expect("input command"));assert_eq!(generation,7);
 match command{Command::Input{conversation_id,draft}=>{assert_eq!(conversation_id,"chat-b");assert_eq!(draft.target,InputTarget::Task{task_id:"task-b".into()});assert_eq!(draft.expected_revision,Some(9));assert_eq!(draft.intent,InputIntent::Continue);assert_eq!(draft.text,"针对 Beta 的补充");},_=>panic!("wrong native action")}
 assert_eq!(h.view.read_with(cx,|view,cx|view.composer.read(cx).value().to_string()),"针对 Beta 的补充","editor must retain input until durable storage acknowledges it");
}
#[gpui_kit::test]
fn an_unselected_non_main_target_does_not_submit(cx:&mut TestAppContext){
 let mut h=open(cx,Route::Chat("chat-a".into()),fixture());h.view.update(cx,|view,cx|{view.target_kind=1;view.selected_target=None;cx.notify();});enter_message(&h,cx,"不要发给第一个任务");click(&h,cx,"send");
 assert!(h.commands.try_recv().is_err());assert_eq!(h.view.read_with(cx,|view,_|view.notice.clone()).as_deref(),Some("请明确选择消息目标。"));
}
#[gpui_kit::test]
fn offline_members_can_save_but_editor_clears_only_after_durable_ack(cx:&mut TestAppContext){
 let mut snapshot=fixture();snapshot.online=false;snapshot.writable=false;snapshot.can_save_locally=true;
 let mut h=open(cx,Route::Chat("chat-a".into()),snapshot);enter_message(&h,cx,"离线待发内容");click(&h,cx,"send");
 let(_,command)=scoped(h.commands.try_recv().expect("offline staging command"));match command{Command::Input{conversation_id,draft}=>{assert_eq!(conversation_id,"chat-a");assert_eq!(draft.text,"离线待发内容");},_=>panic!("wrong offline native action")}
 assert_eq!(h.view.read_with(cx,|view,cx|view.composer.read(cx).value().to_string()),"离线待发内容");assert!(h.events.try_send(Event::InputStored("离线待发内容".into())).is_ok());cx.run_until_parked();assert_eq!(h.view.read_with(cx,|view,cx|view.composer.read(cx).value().to_string()),"");
}
#[gpui_kit::test]
fn local_read_only_session_cannot_stage_chat(cx:&mut TestAppContext){
 let mut snapshot=fixture();snapshot.local_session=true;snapshot.writable=false;snapshot.can_save_locally=false;
 let mut h=open(cx,Route::Chat("chat-a".into()),snapshot);enter_message(&h,cx,"本机会话无成员签名权限");click(&h,cx,"send");assert!(h.commands.try_recv().is_err());assert_eq!(h.view.read_with(cx,|view,cx|view.composer.read(cx).value().to_string()),"本机会话无成员签名权限");
}
#[gpui_kit::test]
fn inbox_choice_keeps_the_question_conversation(cx:&mut TestAppContext){
 let mut snapshot=fixture();snapshot.questions=vec![ui::Question{question_id:"question-b".into(),conversation_id:"chat-b".into(),text:"允许本次操作？".into(),choices:vec![ui::Choice{label:"仅允许一次".into(),action:Some("allow_once".into())}],..Default::default()}];
 let mut h=open(cx,Route::Inbox,snapshot);click(&h,cx,"q-question-b-仅允许一次");let(generation,command)=scoped(h.commands.try_recv().expect("answer command"));assert_eq!(generation,7);match command{Command::Answer{conversation_id,question_id,text,action}=>{assert_eq!(conversation_id,"chat-b");assert_eq!(question_id,"question-b");assert_eq!(text,"仅允许一次");assert_eq!(action.as_deref(),Some("allow_once"));},_=>panic!("wrong answer action")}
}

#[gpui_kit::test]
fn verified_local_node_operator_can_open_and_save_settings_without_network_write(cx:&mut TestAppContext){
 let mut snapshot=fixture();snapshot.local_session=true;snapshot.writable=false;snapshot.can_save_locally=false;snapshot.can_configure=true;
 let mut h=open(cx,Route::Devices,snapshot);click(&h,cx,"node-settings-editor");let(_,command)=scoped(h.commands.try_recv().expect("read node config"));assert!(matches!(command,Command::EditSettings(views::backend::SettingsKind::Node)));
 assert!(h.events.try_send(Event::EditForm{editor:views::forms::Editor::NodeSettings,values:std::collections::BTreeMap::from([("roots","/controlled/project".into()),("provider",String::new()),("backup_automatic","false".into())])}).is_ok());cx.run_until_parked();click(&h,cx,"save-form");let(generation,command)=scoped(h.commands.try_recv().expect("save local node config"));assert_eq!(generation,7);match command{Command::Save{editor,values,..}=>{assert!(matches!(editor,views::forms::Editor::NodeSettings));assert_eq!(values.get("roots").map(String::as_str),Some("/controlled/project"));assert_eq!(values.get("provider").map(String::as_str),Some(""));},_=>panic!("wrong local settings action")}
 assert!(!h.view.read_with(cx,|view,_|view.snapshot.writable));assert!(!h.view.read_with(cx,|view,_|view.snapshot.can_manage));
}
#[gpui_kit::test]
fn non_operator_member_cannot_open_node_configuration(cx:&mut TestAppContext){
 let mut snapshot=fixture();snapshot.can_configure=false;let mut h=open(cx,Route::Devices,snapshot);click(&h,cx,"node-settings-editor");assert!(h.commands.try_recv().is_err());assert!(h.view.read_with(cx,|view,_|view.form.is_none()));
}
#[gpui_kit::test]
fn profile_switch_requires_its_explicit_confirmation(cx:&mut TestAppContext){
 use host::profiles::{PersonalProfile,ProfileHints,ProfileId};let mut snapshot=fixture();let id="00000000-0000-4000-8000-000000000001";snapshot.profile_id=Some("default".into());snapshot.profiles=vec![PersonalProfile{id:ProfileId::parse("default").unwrap(),label:"原网络".into(),hints:ProfileHints::default()},PersonalProfile{id:ProfileId::parse(id).unwrap(),label:"另一个网络".into(),hints:ProfileHints::default()}];let mut h=open(cx,Route::Connect,snapshot);click(&h,cx,&format!("switch-profile-{id}"));assert!(h.commands.try_recv().is_err());assert!(h.view.read_with(cx,|view,_|view.confirmation.is_some()));click(&h,cx,"confirm-danger");match h.commands.try_recv().expect("explicit profile switch"){Command::SwitchProfile(profile)=>assert_eq!(profile,id),_=>panic!("profile switch must not submit a network command")}
}
#[gpui_kit::test]
fn cold_cached_member_can_stage_without_claiming_a_fresh_read(cx:&mut TestAppContext){
 let mut snapshot=fixture();snapshot.online=false;snapshot.synced=false;snapshot.writable=false;snapshot.cached=true;snapshot.can_save_locally=true;snapshot.status="离线缓存，状态尚未重新确认".into();let mut h=open(cx,Route::Chat("chat-a".into()),snapshot);enter_message(&h,cx,"冷启动后的本机待发输入");click(&h,cx,"send");let(_,command)=scoped(h.commands.try_recv().expect("cached target staging"));assert!(matches!(command,Command::Input{..}));assert!(!h.view.read_with(cx,|view,_|view.snapshot.synced));assert!(!h.view.read_with(cx,|view,_|view.snapshot.writable));
}
#[gpui_kit::test]
fn explicit_manual_unlock_uses_the_entered_passphrase_without_requesting_a_vault(cx:&mut TestAppContext){
 let mut h=open(cx,Route::Connect,fixture());h.window.update(cx,|_,window,cx|h.view.update(cx,|view,cx|view.edit(views::forms::Editor::Connect,std::collections::BTreeMap::from([("origin","https://fixture.invalid".into()),("node_id","a".repeat(64)),("unlock_mode","manual".into()),("unlock_password","fixture-unlock-passphrase".into())]),window,cx))).unwrap();click(&h,cx,"save-form");let command=h.commands.try_recv().expect("manual unlock intent");match command{Command::Save{values,editor,..}=>{assert!(matches!(editor,views::forms::Editor::Connect));assert_eq!(values.get("passphrase").map(String::as_str),Some("fixture-unlock-passphrase"));assert!(!values.contains_key("unlock_password"));},_=>panic!("manual unlock must use the explicit identity path")}
}
#[gpui_kit::test]
fn stale_critical_events_cannot_clear_a_newer_draft_or_copy_an_old_profile_secret(cx:&mut TestAppContext){
 let h=open(cx,Route::Chat("chat-a".into()),fixture());enter_message(&h,cx,"当前档案尚未保存的文字");
 assert!(h.events.try_send(Event::Scoped{generation:6,profile_id:None,event:Box::new(Event::InputStored("当前档案尚未保存的文字".into()))}).is_ok());
 assert!(h.events.try_send(Event::Scoped{generation:7,profile_id:Some("other-profile".into()),event:Box::new(Event::CopySecret(zeroize::Zeroizing::new("fixture-old-profile-secret".into())))}).is_ok());cx.run_until_parked();assert_eq!(h.view.read_with(cx,|view,cx|view.composer.read(cx).value().to_string()),"当前档案尚未保存的文字");assert!(cx.update(|cx|cx.read_from_clipboard()).is_none());
}
#[gpui_kit::test]
fn a_verified_aligning_projection_can_stage_locally_without_granting_online_writes(cx:&mut TestAppContext){
 let mut snapshot=fixture();snapshot.synced=false;snapshot.writable=false;snapshot.cached=false;snapshot.projection_valid=true;snapshot.can_save_locally=true;snapshot.write_state="aligning".into();let mut h=open(cx,Route::Chat("chat-a".into()),snapshot);enter_message(&h,cx,"等待状态补齐的输入");click(&h,cx,"send");let(_,command)=scoped(h.commands.try_recv().expect("pure local staging"));assert!(matches!(command,Command::Input{..}));assert!(!h.view.read_with(cx,|view,_|view.snapshot.writable));
}

#[gpui_kit::test]
fn native_history_reads_the_selected_section_with_current_scope(cx:&mut TestAppContext){
 use vibeface_protocol::history::{HistoryCoverage,HistorySection};
 let mut snapshot=fixture();snapshot.conversation=Arc::new(Conversation{history:std::collections::BTreeMap::from([("works".into(),HistoryCoverage{next_cursor:Some("opaque-older".into()),complete:false})]),..(*snapshot.conversation).clone()});
 let mut h=open(cx,Route::History(HistorySection::Works),snapshot);click(&h,cx,"load-history-works");
 let(generation,command)=scoped(h.commands.try_recv().expect("native history page command"));assert_eq!(generation,7);
 match command{Command::LoadHistory{conversation_id,section}=>{assert_eq!(conversation_id,"chat-a");assert_eq!(section,HistorySection::Works);},_=>panic!("history control routed to another action")}
}

#[gpui_kit::test]
fn offline_history_keeps_coverage_without_requesting_a_page(cx:&mut TestAppContext){
 use vibeface_protocol::history::{HistoryCoverage,HistorySection};
 let mut snapshot=fixture();snapshot.online=false;snapshot.cached=true;snapshot.conversation=Arc::new(Conversation{history:std::collections::BTreeMap::from([("memory".into(),HistoryCoverage{next_cursor:Some("old-cursor".into()),complete:false})]),..(*snapshot.conversation).clone()});
 let mut h=open(cx,Route::History(HistorySection::Memory),snapshot);click(&h,cx,"load-history-memory");assert!(h.commands.try_recv().is_err(),"offline history button must not issue a page request");
}

#[gpui_kit::test]
fn overwritten_profile_restore_uses_the_original_change(cx:&mut TestAppContext){
 use vibeface_protocol::history::HistorySection;
 let original=serde_json::json!({"change_id":"my-change","conversation_id":"chat-a","cause":null,"event_hash":"event-a","created_at_ms":10,"change":{"kind":"profile","name":{"before":"Before","after":"My Alpha"}}});
 let record=serde_json::json!({"change_id":"my-change","by":"other-device","field":"name","value":"Their Alpha"});
 let mut snapshot=fixture();snapshot.conversation=Arc::new(Conversation{changes:vec![serde_json::from_value(original).unwrap()],overwritten:vec![serde_json::from_value(record).unwrap()],..(*snapshot.conversation).clone()});
 let mut h=open(cx,Route::History(HistorySection::Overwritten),snapshot);click(&h,cx,"restore-profile-0");assert!(h.commands.try_recv().is_err(),"restoration requires the visible confirmation");click(&h,cx,"confirm-danger");
 let(_,command)=scoped(h.commands.try_recv().expect("confirmed restoration"));match command{Command::RestoreProfile{conversation_id,record}=>{assert_eq!(conversation_id,"chat-a");assert_eq!(record.change_id,"my-change");},_=>panic!("wrong restore command")}
}

#[gpui_kit::test]
fn prepending_messages_and_updating_rows_preserve_the_reader_anchor(cx:&mut TestAppContext){
 let rows=(0..20).map(|index|ui::Message{message_id:format!("message-{index}"),role:"user".into(),text:format!("正文 {index}"),..Default::default()}).collect::<Vec<_>>();
 let h=open(cx,Route::Chat("chat-a".into()),fixture());h.view.update(cx,|view,cx|{view.timeline.replace(rows.clone(),cx);view.timeline.list.scroll_to(gpui_kit::ListOffset{item_ix:8,offset_in_item:gpui_kit::px(17.)});let mut older=vec![ui::Message{message_id:"older".into(),role:"user".into(),text:"更早记录".into(),..Default::default()}];older.extend(rows);view.timeline.replace(older.clone(),cx);assert_eq!(view.timeline.list.logical_scroll_top(),gpui_kit::ListOffset{item_ix:9,offset_in_item:gpui_kit::px(17.)});assert_eq!(view.timeline.message(9).unwrap().message_id,"message-8");older[9].text.push_str("，刷新后的内容");view.timeline.replace(older,cx);assert_eq!(view.timeline.list.logical_scroll_top(),gpui_kit::ListOffset{item_ix:9,offset_in_item:gpui_kit::px(17.)});});
}
