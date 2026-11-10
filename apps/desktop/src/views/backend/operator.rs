use super::*;
use crate::host::{NodeOperatorChange,NodeOperatorRead};
#[derive(Serialize,Deserialize)]
#[serde(deny_unknown_fields)]
struct PendingOperator{node_id:String,operation_id:String}
impl Backend{
 pub(super) fn operator_identity(&self)->Result<NodeIdentity>{let identity=self.host.snapshot().selected.context("请先明确选择由当前用户管理的运行中Node")?;if let Some(expected)=&self.form_owner{ensure!(expected==&identity.device_id,"设置表单的原本机Node已经切换，请重新读取");}Ok(identity)}
 pub(super) async fn read_node(&mut self,query:ReadRequest)->Result<Value>{
  if self.form_owner.is_none(){if let Some(client)=self.client.as_ref(){return client.read(query).await}}
  let identity=self.operator_identity()?;
  let query=match query{ReadRequest::NodeSettings=>NodeOperatorRead::NodeSettings,ReadRequest::TriageSettings=>NodeOperatorRead::TriageSettings,ReadRequest::RelaySettings=>NodeOperatorRead::RelaySettings,ReadRequest::ProviderModels{provider}=>NodeOperatorRead::ProviderModels{provider},ReadRequest::ProviderProbe{provider}=>NodeOperatorRead::ProviderProbe{provider},ReadRequest::Logs=>NodeOperatorRead::Logs{filter:Default::default()},ReadRequest::LogsFiltered(filter)=>NodeOperatorRead::Logs{filter},ReadRequest::Diagnostics=>NodeOperatorRead::Diagnostics{since_ms:None,until_ms:None},_=>anyhow::bail!("此请求不属于本机Node设置")};
  self.host.operator_read(&identity,query).await
 }
 pub(super) async fn read_owner_node(&mut self,query:ReadRequest)->Result<Value>{let previous=self.form_owner.clone();self.form_owner=Some(self.host.snapshot().selected.context("请先选择本机Node")?.device_id);let result=self.read_node(query).await;self.form_owner=previous;result}
 pub(super) async fn operator_submit(&mut self,action:Action)->Result<()>{
  let identity=self.operator_identity()?;let path=self.client_directory()?.join("operator-unknown.json");
  ensure!(!tokio::fs::try_exists(&path).await?,"原本机设置操作结果未知，请先读取配置并明确核对原状态");
  let change=NodeOperatorChange::try_from(action)?;let pending=PendingOperator{node_id:identity.device_id.clone(),operation_id:self.form_id.clone().unwrap_or_else(||uuid::Uuid::new_v4().to_string())};let save_path=path.clone();
  tokio::task::spawn_blocking(move||{vibeface_bridge::platform::privatefs::create_dir(save_path.parent().context("待核对记录缺少父目录")?)?;vibeface_bridge::platform::privatefs::write_atomic(&save_path,&serde_json::to_vec(&pending)?)}).await??;
  self.state.operator_unknown=Some(identity.device_id.clone());self.publish();
  match self.host.operator_change(&identity,change).await{
   Ok(mut value)=>{if let Some(token)=value.as_object_mut().and_then(|object|object.remove("token")).and_then(|value|value.as_str().map(str::to_owned)){let _=self.out.send(Event::CopySecret(zeroize::Zeroizing::new(token))).await;}tokio::fs::remove_file(path).await?;self.state.operator_unknown=None;if let Some(id)=&self.form_id{let _=self.out.send(Event::FormSaved(id.clone())).await;}self.publish();let _=self.out.send(Event::Notice("本机设置操作已返回，读取配置可核对当前状态。".into())).await;Ok(())},
   Err(error)=>{if !error.is::<crate::host::OperatorOutcomeUnknown>(){tokio::fs::remove_file(path).await?;self.state.operator_unknown=None;self.publish();}Err(error)}
  }
 }
 pub(super) async fn acknowledge_operator(&mut self,node_id:&str)->Result<()>{let identity=self.operator_identity()?;ensure!(identity.device_id==node_id,"待核对操作不属于所选Node");self.host.operator_read(&identity,NodeOperatorRead::NodeSettings).await?;let path=self.client_directory()?.join("operator-unknown.json");let bytes=vibeface_bridge::platform::privatefs::read_private_bounded(&path,4096,"本机设置待核对记录")?;let pending:PendingOperator=serde_json::from_slice(&bytes)?;ensure!(pending.node_id==node_id,"待核对操作不属于所选Node");tokio::fs::remove_file(path).await?;self.state.operator_unknown=None;self.publish();let _=self.out.send(Event::Notice("已关闭本机设置待核对提醒。原请求不会重发；新的操作需要重新明确发起。".into())).await;Ok(())}
 pub(super) async fn load_operator_pending(&mut self)->Result<()>{let path=self.client_directory()?.join("operator-unknown.json");if tokio::fs::try_exists(&path).await?{let pending:PendingOperator=serde_json::from_slice(&vibeface_bridge::platform::privatefs::read_private_bounded(&path,4096,"本机设置待核对记录")?)?;self.state.operator_unknown=Some(pending.node_id);self.publish();}Ok(())}
}
