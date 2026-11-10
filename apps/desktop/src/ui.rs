//! Immutable native view models. Wire parsing runs on the client task, never in render.
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use vibeface_protocol::workspace::{Attachment, InputIntent, InputTarget};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Route { Connect, Join, Home, Chat(String), Inbox, Tasks, Channels, Memory, Schedules, Bot, Devices, Settings, Node, Logs, Transfers }
impl Route {
    pub fn title(&self) -> &'static str { match self { Self::Connect=>"连接个人网络",Self::Join=>"加入与恢复",Self::Home=>"Bot",Self::Chat(_)=>"对话",Self::Inbox=>"待处理",Self::Tasks=>"任务",Self::Channels=>"频道",Self::Memory=>"记忆",Self::Schedules=>"计划",Self::Bot=>"Bot 设置",Self::Devices=>"设备与成员",Self::Settings=>"设置",Self::Node=>"本机 Node",Self::Logs=>"日志",Self::Transfers=>"文件传输" } }
}
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct Bot {#[serde(default)]pub avatar:Option<vibeface_client::Avatar>,#[serde(default)]pub avatar_version:Option<String>, pub bot_id:String,pub conversation_id:String,pub name:String,#[serde(default)]pub instructions:String,#[serde(default)]pub project:Option<Project>,#[serde(default)]pub models:BTreeMap<String,String>,#[serde(default)]pub busy:Option<String> }
#[derive(Clone,Debug,Default,Deserialize,Serialize)]
pub struct Project{pub target_node:String,pub cwd:String}
#[derive(Clone,Debug,Default,Deserialize,Serialize)]
pub struct Message {#[serde(default)]pub target:Option<InputTarget>,#[serde(default)]pub rank:Option<String>,#[serde(default)]pub in_reply_to:Option<String>,#[serde(default)]pub created_at_ms:i64,pub message_id:String,pub role:String,#[serde(default)]pub text:String,#[serde(default)]pub event_hash:String,#[serde(default)]pub attachments:Vec<Attachment>,#[serde(default)]pub status:Option<String>}
#[derive(Clone,Debug,Default,Deserialize,Serialize)]
pub struct Work {pub work_id:String,#[serde(default)]pub title:String,#[serde(default)]pub summary:String,#[serde(default)]pub state:String,#[serde(default)]pub result_text:String,#[serde(default)]pub target_node:String}
#[derive(Clone,Debug,Default,Deserialize,Serialize)]
pub struct Question {#[serde(default)]pub target:Option<InputTarget>,#[serde(default)]pub closed:Option<String>,pub question_id:String,#[serde(default)]pub conversation_id:String,#[serde(default)]pub text:String,#[serde(default)]pub title:String,#[serde(default)]pub choices:Vec<Choice>,#[serde(default)]pub answer:Option<serde_json::Value>}
#[derive(Clone,Debug,Default,Deserialize,Serialize)]
pub struct Choice{pub label:String,#[serde(default)]pub action:Option<String>}
#[derive(Clone,Debug,Default,Deserialize,Serialize)]
pub struct Task {#[serde(default)]pub channel_id:Option<String>,pub task_id:String,pub title:String,pub owner_bot:String,#[serde(default)]pub request:String,#[serde(default)]pub expected_result:String,#[serde(default)]pub dependencies:Vec<String>,pub revision:u64,#[serde(default)]pub state:String}
#[derive(Clone,Debug,Default,Deserialize,Serialize)]
pub struct Channel {pub channel_id:String,pub title:String,pub purpose:String,pub members:Vec<String>,pub coordinator_bot:String,pub coordinator_node:String,pub revision:u64,pub owner_epoch:u64}
#[derive(Clone,Debug,Default,Deserialize,Serialize)]
pub struct Node {pub device_id:String,pub name:String,#[serde(default)]pub online:bool,#[serde(default)]pub revoked:bool,#[serde(default)]pub local:bool}
#[derive(Clone,Debug,Default,Deserialize,Serialize)]
pub struct Member{pub device_id:String,pub name:String,pub kind:String,#[serde(default)]pub manage:bool}
#[derive(Clone,Debug,Default,Deserialize,Serialize)]
pub struct Memory {pub memory_id:String,pub text:String,pub metadata:MemoryMetadata}
#[derive(Clone,Debug,Default,Deserialize,Serialize)]
pub struct MemoryMetadata{pub epoch:u64,pub revision:String,pub control_revision:u64,pub source_event_hash:String,#[serde(default)]pub user_controlled:bool}
#[derive(Clone,Debug,Default,Deserialize,Serialize)]
pub struct Schedule {pub schedule_id:String,pub name:String,pub prompt:String,pub enabled:bool,pub first_run_at_ms:i64,#[serde(default)]pub every_minutes:Option<u64>,#[serde(default)]pub next_run_at_ms:Option<i64>}
#[derive(Clone,Debug,Default,Deserialize,Serialize)]
pub struct Scheduler {#[serde(default)]pub jobs:Vec<Job>,#[serde(default)]pub scope_jobs:Vec<ScopeJob>}
#[derive(Clone,Debug,Default,Deserialize,Serialize)]
pub struct Job{pub trigger:String,pub state:String,pub scope:Scope}
#[derive(Clone,Debug,Default,Deserialize,Serialize)]
#[serde(tag="kind",rename_all="snake_case")]
pub enum Scope{#[default]Main,WorkspaceTask{task_id:String},Task{work_id:String},Routine{trigger:String}}
#[derive(Clone,Debug,Default,Deserialize,Serialize)]
pub struct ScopeJob{pub conversation_id:String,pub trigger:String,pub scope:Scope}
#[derive(Clone,Debug,Default,Deserialize,Serialize)]
pub struct ConversationIdentity{pub conversation_id:String,#[serde(default)]pub owner:Option<String>,pub owner_epoch:u64}
#[derive(Clone,Debug,Default,Deserialize,Serialize)]
pub struct Conversation {#[serde(default)]pub checks:Vec<CoordinationCheck>,#[serde(default)]pub conversation:Option<ConversationIdentity>,#[serde(default,deserialize_with="deserialize_scheduler")]pub scheduler:Scheduler,#[serde(default)]pub messages:Vec<Message>,#[serde(default)]pub works:Vec<Work>,#[serde(default)]pub questions:Vec<Question>,#[serde(default)]pub bot_settings:BotSettings}
#[derive(Clone,Debug,Default,Deserialize,Serialize)]
pub struct BotSettings{#[serde(default)]pub skills:Vec<Skill>,#[serde(default)]pub memory_sync_conflicts:MemorySyncConflicts,#[serde(default)]pub memory_staged_count:Option<u64>,#[serde(default)]pub memory_conflicts:Option<Vec<MemoryConflict>>,#[serde(default)]pub memory:Vec<Memory>,#[serde(default)]pub schedules:Vec<Schedule>}
#[derive(Clone,Debug,Default)]
pub struct Snapshot {pub generation:u64,pub online:bool,pub synced:bool,pub writable:bool,pub local_session:bool,pub write_state:String,pub network:Option<String>,pub client_id:Option<String>,pub can_manage:bool,pub bots:Vec<Bot>,pub nodes:Vec<Node>,pub members:Vec<Member>,pub joins:Vec<JoinRequest>,pub inputs:Vec<vibeface_protocol::workspace::QueuedInput>,pub tasks:Vec<Task>,pub channels:Vec<Channel>,pub conversation:std::sync::Arc<Conversation>,pub pending:Vec<Pending>,pub identity_plan:Option<IdentityPlanView>,pub drafts:Vec<NativeDraftSummary>,pub questions:Vec<Question>,pub notifications:Vec<Notification>,pub live:Vec<LiveProgress>,pub delivery:Delivery,pub status:String,pub error:Option<String>}
#[derive(Clone,Debug)]
pub struct Pending{pub id:String,pub state:String}
#[derive(Clone,Debug,Deserialize,Serialize)]
pub struct ComposerDraft{pub target:InputTarget,pub intent:InputIntent,pub text:String,pub attachments:Vec<Attachment>,pub expected_revision:Option<u64>}
impl ComposerDraft {
    pub fn validate(&self)->Result<(),String>{ if self.text.trim().is_empty()&&self.attachments.is_empty(){return Err("请输入消息或添加附件".into())} if self.text.len()>4*1024*1024{return Err("消息超过 4 MiB，请作为附件发送".into())} Ok(()) }
}
pub fn decode_list<T:serde::de::DeserializeOwned>(value:&serde_json::Value,key:&str)->anyhow::Result<Vec<T>> { let array=if value.is_array(){value.clone()}else{value.get(key).cloned().ok_or_else(||anyhow::anyhow!("响应缺少 {key}"))?};Ok(serde_json::from_value(array)?) }

fn deserialize_scheduler<'de,D:serde::Deserializer<'de>>(d:D)->Result<Scheduler,D::Error>{Ok(Option::<Scheduler>::deserialize(d)?.unwrap_or_default())}

#[derive(Clone,Debug,Default,Deserialize,Serialize)]
pub struct Delivery{#[serde(default)]pub enabled:bool,#[serde(default)]pub charity_enabled:bool,#[serde(default)]pub paid_enabled:bool,#[serde(default)]pub max_file_bytes:u64,#[serde(default)]pub orders:Vec<TransferOrder>}
#[derive(Clone,Debug,Default,Deserialize,Serialize)]
pub struct TransferOrder{pub order_id:String,pub intent_id:String,pub role:String,pub mode:String,pub acceptance:String,pub source_peer:String,pub receiver_peer:String,pub provider_peer:String,#[serde(default)]pub name:Option<String>,#[serde(default)]pub bytes:u64,#[serde(default)]pub sha256:Option<String>,pub phase:String,#[serde(default)]pub payment:Payment,#[serde(default)]pub error:Option<String>}
#[derive(Clone,Debug,Default,Deserialize,Serialize,PartialEq)]
pub struct Payment{#[serde(default)]pub state:String,#[serde(default)]pub total:String,#[serde(default)]pub gas_cap:String,#[serde(default)]pub payer:Option<String>,#[serde(default)]pub provider_wallet:Option<String>,#[serde(default)]pub tx_hash:Option<String>}

#[derive(Clone,Debug,Default,Deserialize,Serialize)]
pub struct JoinRequest{pub join_id:String,pub name:String,pub device_id:String,#[serde(default)]pub code:Option<String>}

pub fn parse_conversation(value:serde_json::Value,expected_id:&str,bot_id:Option<&str>)->anyhow::Result<Conversation>{
 let mut conversation:Conversation=serde_json::from_value(value)?;
 anyhow::ensure!(conversation.conversation.as_ref().map(|c|c.conversation_id.as_str())==Some(expected_id),"设备返回了别的对话，已暂停");
 anyhow::ensure!(conversation.scheduler.scope_jobs.iter().all(|job|job.conversation_id==expected_id),"目标任务观察属于别的对话");
 anyhow::ensure!(conversation.questions.iter().all(|q|q.conversation_id==expected_id),"问题属于别的对话");
 for message in &conversation.messages{anyhow::ensure!(!message.message_id.is_empty(),"消息缺少稳定标识");if let(Some(InputTarget::BotMain{bot_id:target}),Some(expected))=(&message.target,bot_id){anyhow::ensure!(target==expected,"消息指向其他 Bot");}}
 conversation.scheduler.jobs.retain(|job|matches!(job.scope,Scope::Main));
 conversation.messages.retain(|message|message.target.as_ref().is_none_or(|target|matches!(target,InputTarget::BotMain{..})));
 conversation.messages.sort_by(|a,b|match(&a.rank,&b.rank){(Some(a),Some(b))=>a.cmp(b),(Some(_),None)=>std::cmp::Ordering::Less,(None,Some(_))=>std::cmp::Ordering::Greater,_=>std::cmp::Ordering::Equal});
 conversation.questions.retain(|q|matches!(q.target,Some(InputTarget::BotMain{..}))&&q.closed.is_none());
 Ok(conversation)
}
#[derive(Clone,Debug,Default)]
pub struct LiveProgress{pub key:String,pub conversation_id:String,pub sequence:u64,pub text:String,pub status:String,pub invocation:Option<String>,pub items:Vec<(String,String)>}
pub fn apply_progress(streams:&mut Vec<LiveProgress>,event:&serde_json::Value)->anyhow::Result<bool>{
 let kind=event["type"].as_str().unwrap_or("");anyhow::ensure!(["manager_progress","native_progress"].contains(&kind),"未知进度类型");
 let field=|key:&str|event[key].as_str().filter(|s|!s.is_empty()).ok_or_else(||anyhow::anyhow!("进度缺少 {key}"));
 let conversation=field("conversation_id")?;let key=if kind=="manager_progress"{format!("manager:{conversation}:{}:{}",field("source_node")?,field("trigger")?)}else{format!("native:{conversation}:{}:{}",field("target_node")?,field("work_id")?)};
 let sequence=event["sequence"].as_u64().filter(|seq|*seq>0).ok_or_else(||anyhow::anyhow!("进度序号无效"))?;let invocation=event["invocation_id"].as_str().map(str::to_owned);
 if !streams.iter().any(|s|s.key==key){streams.push(LiveProgress{key:key.clone(),conversation_id:conversation.into(),..Default::default()});}
 let current=streams.iter_mut().find(|s|s.key==key).expect("inserted stream");if current.invocation!=invocation{*current=LiveProgress{key,conversation_id:conversation.into(),invocation:invocation.clone(),..Default::default()};}
 if sequence<=current.sequence{return Ok(false)}let gap=sequence>current.sequence+1;let progress=&event["progress"];
 match progress["type"].as_str(){Some("text_snapshot"|"text_delta")=>{let text=progress["text"].as_str().ok_or_else(||anyhow::anyhow!("进度文本无效"))?;let item_id=progress["item_id"].as_str().unwrap_or("message");if !current.items.iter().any(|(id,_)|id==item_id){current.items.push((item_id.into(),String::new()));}let item=current.items.iter_mut().find(|(id,_)|id==item_id).expect("inserted item");if progress["type"]=="text_snapshot"{item.1=text.into()}else if !gap{item.1.push_str(text)}current.text=current.items.iter().map(|(_,text)|text.as_str()).collect::<Vec<_>>().join("\n\n");},Some("status"|"tool")=>{current.status=progress["text"].as_str().or_else(||progress["status"].as_str()).or_else(||progress["name"].as_str()).unwrap_or("状态待确认").into();},_=>anyhow::bail!("进度内容无效")};current.sequence=sequence;Ok(gap)
}

#[derive(Clone,Debug,Default,Deserialize,Serialize)]
pub struct MemorySyncConflicts{#[serde(default)]pub items:Vec<MemorySyncConflict>,#[serde(default)]pub has_more:bool}
#[derive(Clone,Debug,Default,Deserialize,Serialize)]
pub struct MemorySyncConflict{pub memory_id:String,pub losing_revision:String,pub winning_revision:String,pub reason:String}
#[derive(Clone,Debug,Default,Deserialize,Serialize)]
pub struct MemoryConflict{pub memory_id:String,pub expected_revision:String,pub actual_revision:String,pub reason:String}

pub fn target_conversation(target:&InputTarget,shown:&Snapshot,current:Option<&str>)->anyhow::Result<String>{
 let owner=match target{InputTarget::BotMain{bot_id}|InputTarget::Routine{bot_id,..}|InputTarget::ChannelMember{bot_id,..}|InputTarget::ChannelTask{bot_id,..}=>Some(bot_id.as_str()),InputTarget::Task{task_id}=>Some(shown.tasks.iter().find(|t|&t.task_id==task_id).ok_or_else(||anyhow::anyhow!("目标任务未加载"))?.owner_bot.as_str()),InputTarget::Work{work_id}=>{anyhow::ensure!(shown.conversation.works.iter().any(|work|&work.work_id==work_id),"目标工作不属于当前对话");return current.map(str::to_owned).ok_or_else(||anyhow::anyhow!("工作对话未选择"));}};
 let bot=shown.bots.iter().find(|bot|Some(bot.bot_id.as_str())==owner).ok_or_else(||anyhow::anyhow!("目标 Bot 未加载"))?;Ok(bot.conversation_id.clone())
}

#[derive(Clone,Debug)]pub struct NativeDraftSummary{pub fingerprint:String,pub text:String,pub conversation_id:String}
#[derive(Clone,Debug,Default,Deserialize)]
pub struct ConversationRecords{pub conversation_id:String,#[serde(default)]pub messages:Vec<Message>,#[serde(default)]pub works:Vec<Work>,#[serde(default)]pub questions:Vec<Question>,#[serde(default)]pub jobs:Vec<Job>,#[serde(default)]pub scope_jobs:Vec<ScopeJob>,#[serde(default)]pub memory:Option<Vec<Memory>>,#[serde(default)]pub memory_sync_conflicts:Option<MemorySyncConflicts>,#[serde(default)]pub memory_staged_count:Option<u64>,#[serde(default)]pub memory_conflicts:Option<Vec<MemoryConflict>>}
pub fn merge_records(shown:&Conversation,records:ConversationRecords)->anyhow::Result<Conversation>{
 let expected=shown.conversation.as_ref().ok_or_else(||anyhow::anyhow!("对话尚未首读"))?.conversation_id.as_str();
 anyhow::ensure!(records.conversation_id==expected&&records.scope_jobs.iter().all(|job|job.conversation_id==expected)&&records.questions.iter().all(|q|q.conversation_id==expected),"推送记录属于其他对话");
 let mut next=shown.clone();
 for message in records.messages.into_iter().filter(|m|m.target.as_ref().is_none_or(|t|matches!(t,InputTarget::BotMain{..}))){if let Some(old)=next.messages.iter_mut().find(|old|old.message_id==message.message_id){*old=message}else{next.messages.push(message)}}
 next.messages.sort_by(|a,b|match(&a.rank,&b.rank){(Some(a),Some(b))=>a.cmp(b),(Some(_),None)=>std::cmp::Ordering::Less,(None,Some(_))=>std::cmp::Ordering::Greater,_=>std::cmp::Ordering::Equal});
 for work in records.works{if let Some(old)=next.works.iter_mut().find(|old|old.work_id==work.work_id){*old=work}else{next.works.push(work)}}
 next.scheduler.jobs=records.jobs.into_iter().filter(|job|matches!(job.scope,Scope::Main)).collect();next.scheduler.scope_jobs=records.scope_jobs;
 next.questions=records.questions.into_iter().filter(|q|matches!(q.target,Some(InputTarget::BotMain{..}))&&q.closed.is_none()).collect();
 if let Some(memory)=records.memory{next.bot_settings.memory=memory;}if let Some(conflicts)=records.memory_sync_conflicts{next.bot_settings.memory_sync_conflicts=conflicts;}if let Some(count)=records.memory_staged_count{next.bot_settings.memory_staged_count=Some(count);}next.bot_settings.memory_conflicts=records.memory_conflicts;
 Ok(next)
}

#[derive(Clone,Debug,Default,Deserialize,Serialize)]pub struct CoordinationCheck{pub check_id:String,pub conversation_id:String,pub trigger:String,pub attempt_id:String,pub detail:String}
#[derive(Clone,Debug,Default,Deserialize,Serialize)]pub struct Notification{pub notification_id:String,pub kind:String,pub text:String,pub conversation_id:String,#[serde(default)]pub target:Option<InputTarget>}

#[derive(Clone,Debug,Default,Deserialize,Serialize)]pub struct Skill{pub skill_id:String,pub name:String,pub description:String}

#[derive(Clone,Debug)]pub struct IdentityPlanView{pub backup_path:String,pub sha256:String,pub network_id:String,pub state:String}
