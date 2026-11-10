use super::*;

impl Backend {
 pub(super) async fn save_delivery(&mut self,values:BTreeMap<String,String>)->Result<()>{
  #[cfg(test)] Self::test_phase("save-delivery-enter");
  let get=|key:&str|values.get(key).cloned().unwrap_or_default();
  {ensure!(self.state.can_manage,"仅管理设备可准备订单");let path=PathBuf::from(get("file"));let metadata=tokio::fs::metadata(&path).await?;ensure!(self.state.delivery.enabled&&metadata.len()<=self.state.delivery.max_file_bytes,"传输未开启或文件超过允许大小");let name=path.file_name().context("文件名无效")?.to_string_lossy().into_owned();let content=tokio::fs::read(&path).await?;ensure!(content.len()as u64==metadata.len(),"文件读取过程中大小改变");let mode: vibeface_client::DeliveryMode=serde_json::from_value(json!(get("mode")))?;let acceptance:vibeface_client::DeliveryAcceptance=serde_json::from_value(json!(get("acceptance")))?;let intent_id=self.delivery_intent(json!({"receiver":get("receiver_peer"),"provider":get("provider_peer"),"mode":get("mode"),"acceptance":get("acceptance"),"name":name,"bytes":content.len(),"sha256":format!("{:x}",Sha256::digest(&content)),"service_cap":get("max_service_amount"),"gas_cap":get("max_gas_amount")})).await?;self.submit(Action::Delivery(vibeface_client::DeliveryAction::Prepare(vibeface_client::DeliveryPrepare{intent_id,receiver_peer:get("receiver_peer"),provider_peer:get("provider_peer"),mode,acceptance,name,content_base64:base64::engine::general_purpose::STANDARD.encode(content),max_service_amount:get("max_service_amount"),max_gas_amount:get("max_gas_amount")}))).await?;}
  Ok(())
 }
}
