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
pub struct Bot { pub bot_id:String,pub conversation_id:String,pub name:String,#[serde(default)]pub instructions:String,#[serde(default)]pub project:Option<Project>,#[serde(default)]pub models:BTreeMap<String,String>,#[serde(default)]pub busy:Option<String> }
#[derive(Clone,Debug,Default,Deserialize,Serialize)]
pub struct Project{pub target_node:String,pub cwd:String}
#[derive(Clone,Debug,Default,Deserialize,Serialize)]
pub struct Message {pub message_id:String,pub role:String,#[serde(default)]pub text:String,#[serde(default)]pub event_hash:String,#[serde(default)]pub attachments:Vec<Attachment>,#[serde(default)]pub status:Option<String>}
#[derive(Clone,Debug,Default,Deserialize,Serialize)]
pub struct Work {pub work_id:String,#[serde(default)]pub title:String,#[serde(default)]pub summary:String,#[serde(default)]pub state:String,#[serde(default)]pub result_text:String,#[serde(default)]pub target_node:String}
#[derive(Clone,Debug,Default,Deserialize,Serialize)]
pub struct Question {pub question_id:String,#[serde(default)]pub conversation_id:String,#[serde(default)]pub text:String,#[serde(default)]pub title:String,#[serde(default)]pub choices:Vec<Choice>,#[serde(default)]pub answer:Option<serde_json::Value>}
#[derive(Clone,Debug,Default,Deserialize,Serialize)]
pub struct Choice{pub label:String,#[serde(default)]pub action:Option<String>}
#[derive(Clone,Debug,Default,Deserialize,Serialize)]
pub struct Task {pub task_id:String,pub title:String,pub owner_bot:String,#[serde(default)]pub request:String,#[serde(default)]pub expected_result:String,#[serde(default)]pub dependencies:Vec<String>,pub revision:u64,#[serde(default)]pub state:String}
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
pub struct Scheduler {#[serde(default)]pub jobs:Vec<Job>}
#[derive(Clone,Debug,Default,Deserialize,Serialize)]
pub struct Job{pub trigger:String,pub state:String}
#[derive(Clone,Debug,Default,Deserialize,Serialize)]
pub struct Conversation {#[serde(default,deserialize_with="deserialize_scheduler")]pub scheduler:Scheduler,#[serde(default)]pub messages:Vec<Message>,#[serde(default)]pub works:Vec<Work>,#[serde(default)]pub questions:Vec<Question>,#[serde(default)]pub bot_settings:BotSettings}
#[derive(Clone,Debug,Default,Deserialize,Serialize)]
pub struct BotSettings{#[serde(default)]pub memory:Vec<Memory>,#[serde(default)]pub schedules:Vec<Schedule>}
#[derive(Clone,Debug,Default)]
pub struct Snapshot {pub generation:u64,pub online:bool,pub synced:bool,pub network:Option<String>,pub client_id:Option<String>,pub can_manage:bool,pub bots:Vec<Bot>,pub nodes:Vec<Node>,pub members:Vec<Member>,pub joins:Vec<JoinRequest>,pub inputs:Vec<vibeface_protocol::workspace::QueuedInput>,pub tasks:Vec<Task>,pub channels:Vec<Channel>,pub conversation:Conversation,pub pending:Vec<Pending>,pub delivery:Delivery,pub status:String,pub error:Option<String>}
#[derive(Clone,Debug)]
pub struct Pending{pub id:String,pub state:String}
#[derive(Clone,Debug)]
pub struct ComposerDraft{pub target:InputTarget,pub intent:InputIntent,pub text:String,pub attachments:Vec<Attachment>,pub expected_revision:Option<u64>}
impl ComposerDraft {
    pub fn validate(&self)->Result<(),String>{ if self.text.trim().is_empty()&&self.attachments.is_empty(){return Err("请输入消息或添加附件".into())} if self.text.len()>4*1024*1024{return Err("消息超过 4 MiB，请作为附件发送".into())} Ok(()) }
}
pub fn decode_list<T:serde::de::DeserializeOwned>(value:&serde_json::Value,key:&str)->anyhow::Result<Vec<T>> { let array=if value.is_array(){value.clone()}else{value.get(key).cloned().ok_or_else(||anyhow::anyhow!("响应缺少 {key}"))?};Ok(serde_json::from_value(array)?) }

fn deserialize_scheduler<'de,D:serde::Deserializer<'de>>(d:D)->Result<Scheduler,D::Error>{Ok(Option::<Scheduler>::deserialize(d)?.unwrap_or_default())}

#[derive(Clone,Debug,Default,Deserialize,Serialize)]
pub struct Delivery{#[serde(default)]pub enabled:bool,#[serde(default)]pub charity_enabled:bool,#[serde(default)]pub paid_enabled:bool,#[serde(default)]pub max_file_bytes:u64,#[serde(default)]pub orders:Vec<TransferOrder>}
#[derive(Clone,Debug,Default,Deserialize,Serialize)]
pub struct TransferOrder{pub order_id:String,pub intent_id:String,pub role:String,pub mode:String,pub acceptance:String,pub source_peer:String,pub receiver_peer:String,pub provider_peer:String,#[serde(default)]pub name:String,#[serde(default)]pub bytes:u64,#[serde(default)]pub sha256:String,pub phase:String,#[serde(default)]pub payment:Payment,#[serde(default)]pub error:Option<String>}
#[derive(Clone,Debug,Default,Deserialize,Serialize,PartialEq)]
pub struct Payment{#[serde(default)]pub state:String,#[serde(default)]pub total:String,#[serde(default)]pub gas_cap:String,#[serde(default)]pub payer:Option<String>,#[serde(default)]pub provider_wallet:Option<String>,#[serde(default)]pub tx_hash:Option<String>}

#[derive(Clone,Debug,Default,Deserialize,Serialize)]
pub struct JoinRequest{pub join_id:String,pub name:String,pub device_id:String,#[serde(default)]pub code:Option<String>}
