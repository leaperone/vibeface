use super::*;
use crate::host::profiles::{ProfileHints,ProfileId};
impl Backend{
 pub(super) fn client_directory(&self)->Result<PathBuf>{let registry=self.client_profiles.as_ref().context("个人网络档案尚未安全读取")?;Ok(registry.paths(registry.selected_id())?.client_dir)}
 pub(super) fn current_profile_id(&self)->Result<String>{Ok(self.client_profiles.as_ref().context("个人网络档案尚未安全读取")?.selected_id().as_str().into())}
 pub(super) fn publish_profiles(&mut self)->Result<()>{ensure!(self.client_profiles.is_some(),"个人网络档案尚未安全读取");self.publish();Ok(())}
 pub(super) fn remember_profile_hints(&mut self)->Result<()>{let registry=self.client_profiles.as_mut().context("个人网络档案尚未安全读取")?;let id=registry.selected_id().clone();let hints=ProfileHints{network:self.state.network.clone(),entry:self.active_entry.clone().filter(|entry|!entry.origin.is_empty())};registry.set_hints(&id,hints)}
 pub(super) async fn switch_profile(&mut self,id:&str)->Result<()>{let id=ProfileId::parse(id)?;let current=self.current_profile_id()?;let registry=self.client_profiles.as_ref().context("个人网络档案尚未安全读取")?;registry.paths(&id)?;if registry.selected_id()==&id{return Ok(())}if let Some(client)=self.client.take(){client.close();self.retired_clients.insert(current,client);}self.key=None;self.active_entry=None;self.reconnect=None;self.pushes=None;self.identity_plan=None;self.active_record=ActiveRecord::None;self.conversation=None;self.attachments.clear();self.form_id=None;
  self.state=Snapshot{generation:self.state.generation+1,status:"身份档案已切换，原身份、恢复计划与待发操作保留。连接后重新验证目录，不会自动发送旧档案。".into(),..Default::default()};let selected=self.client_profiles.as_mut().context("档案注册表不可用")?.select(&id);self.publish_profiles()?;selected?;self.load_operator_pending().await?;Ok(())
 }
}
