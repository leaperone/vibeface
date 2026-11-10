//! A bounded private projection for display and target selection. It never grants authority.
use crate::ui::*;
use anyhow::{Result,ensure,Context};
use serde::{Deserialize,Serialize};
use std::{collections::HashSet,sync::Arc};
use vibeface_protocol::workspace::Assignment;
pub const MAX_CACHE_BYTES:usize=1024*1024;
#[derive(Clone,Debug,PartialEq,Eq)]
pub struct CacheScope{pub profile_id:String,pub network_id:String,pub client_id:String,pub target_node:String}
#[derive(Clone,Debug,Serialize,Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OfflineProjection{
 version:u32,profile_id:String,network_id:String,client_id:String,target_node:String,
 observed_at_ms:u64,write_state:String,omitted:usize,
 bots:Vec<Bot>,nodes:Vec<Node>,tasks:Vec<Task>,channels:Vec<Channel>,assignments:Vec<Assignment>,
 conversation_id:Option<String>,conversation:Conversation,questions:Vec<Question>,notifications:Vec<Notification>,
}
pub fn encode(snapshot:&Snapshot,scope:&CacheScope,conversation_id:Option<&str>)->Result<Vec<u8>>{
 ensure!(snapshot.online&&snapshot.synced&&snapshot.network.as_deref()==Some(scope.network_id.as_str())&&snapshot.client_id.as_deref()==Some(scope.client_id.as_str()),"只有当前已验证读取才能更新离线视图");
 let mut cache=OfflineProjection{version:1,profile_id:scope.profile_id.clone(),network_id:scope.network_id.clone(),client_id:scope.client_id.clone(),target_node:scope.target_node.clone(),observed_at_ms:vibeface_protocol::now_ms(),write_state:snapshot.write_state.clone(),omitted:0,bots:snapshot.bots.clone(),nodes:snapshot.nodes.clone(),tasks:snapshot.tasks.clone(),channels:snapshot.channels.clone(),assignments:snapshot.assignments.clone(),conversation_id:conversation_id.map(str::to_owned),conversation:(*snapshot.conversation).clone(),questions:snapshot.questions.clone(),notifications:snapshot.notifications.clone()};
 loop{let bytes=serde_json::to_vec(&cache)?;if bytes.len()<=MAX_CACHE_BYTES{return Ok(bytes)}cache.omitted+=1;
  if !cache.conversation.messages.is_empty(){cache.conversation.messages.remove(0);}
  else if !cache.conversation.works.is_empty(){cache.conversation.works.pop();}
  else if !cache.conversation.bot_settings.memory.is_empty(){cache.conversation.bot_settings.memory.pop();}
  else if !cache.tasks.is_empty(){let removed=cache.tasks.pop().expect("nonempty tasks");cache.assignments.retain(|assignment|assignment.task_id!=removed.task_id);}
  else if !cache.channels.is_empty(){cache.channels.pop();}
  else if !cache.assignments.is_empty(){cache.assignments.pop();}
  else if !cache.questions.is_empty(){cache.questions.pop();}
  else if !cache.notifications.is_empty(){cache.notifications.pop();}
  else if !cache.conversation.bot_settings.schedules.is_empty(){cache.conversation.bot_settings.schedules.pop();}
  else if cache.bots.len()>1{let selected=cache.conversation_id.as_deref();let index=cache.bots.iter().position(|bot|Some(bot.conversation_id.as_str())!=selected).unwrap_or(cache.bots.len()-1);cache.bots.remove(index);}
  else{anyhow::bail!("离线视图超过1MiB上限，保留上一次完整可验证缓存");}
 }
}
pub fn decode(bytes:&[u8],scope:&CacheScope)->Result<OfflineProjection>{
 ensure!(bytes.len()<=MAX_CACHE_BYTES,"离线视图超过读取上限");let mut cache:OfflineProjection=serde_json::from_slice(bytes).context("离线视图损坏，未加载")?;
 ensure!(cache.version==1&&cache.profile_id==scope.profile_id&&cache.network_id==scope.network_id&&cache.client_id==scope.client_id&&cache.target_node==scope.target_node,"离线视图属于其他档案、网络、身份或Node");
 let mut ids=HashSet::new();let mut conversations=HashSet::new();for bot in &cache.bots{ensure!(!bot.bot_id.is_empty()&&!bot.conversation_id.is_empty()&&ids.insert(bot.bot_id.clone())&&conversations.insert(bot.conversation_id.clone()),"离线Bot标识重复或缺失");}
 ensure!(cache.tasks.iter().all(|task|ids.contains(&task.owner_bot)),"离线任务缺少原Bot归属");
 if let Some(id)=&cache.conversation_id{let bot=cache.bots.iter().find(|bot|&bot.conversation_id==id).context("离线对话缺少原Bot归属")?;cache.conversation=parse_conversation(serde_json::to_value(&cache.conversation)?,id,Some(&bot.bot_id))?;}else{ensure!(cache.conversation.conversation.is_none(),"离线对话范围不一致");}
 ensure!(["writable","read_only","aligning","forked"].contains(&cache.write_state.as_str()),"离线写入状态无效");Ok(cache)
}
impl OfflineProjection{
 pub fn conversation_id(&self)->Option<&str>{self.conversation_id.as_deref()}
 pub fn into_snapshot(self,generation:u64,can_save:bool)->Snapshot{let can_save_locally=can_save&&self.write_state=="writable";Snapshot{generation,online:false,synced:false,writable:false,can_save_locally,cached:true,cached_at_ms:Some(self.observed_at_ms),cache_omitted:self.omitted,network:Some(self.network_id),client_id:Some(self.client_id),write_state:self.write_state,bots:self.bots,nodes:self.nodes,tasks:self.tasks,channels:self.channels,assignments:self.assignments,conversation:Arc::new(self.conversation),questions:self.questions,notifications:self.notifications,status:format!("离线缓存，读取于 {}。设备、进度与结果尚未重新确认；{}项历史因大小上限未缓存。输入只存本机，等待连接或核对回执。",self.observed_at_ms,self.omitted),..Default::default()}}
}
