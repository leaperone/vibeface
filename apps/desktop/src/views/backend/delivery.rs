use super::*;
#[derive(Clone,Deserialize,Serialize)]
#[serde(deny_unknown_fields)]
struct TransferTerms{receiver:String,provider:String,mode:String,acceptance:String,name:String,bytes:u64,sha256:String,service_cap:String,gas_cap:String}
#[derive(Clone,Deserialize,Serialize)]
#[serde(deny_unknown_fields)]
struct TransferIntent{intent_id:String,network_id:String,client_id:String,target_node:String,terms:TransferTerms}
impl Backend{
 pub(super) async fn refresh_delivery(&mut self)->Result<()>{
  let client=self.client()?;let status=client.read(ReadRequest::Delivery(DeliveryRead::Status)).await?;let list=client.read(ReadRequest::Delivery(DeliveryRead::List)).await?;
  let mut delivery:Delivery=serde_json::from_value(status)?;let target=self.active_entry.as_ref().context("当前设备未确认")?.node_id.as_str();ensure!(delivery.node_peer==target,"文件传输状态不属于当前设备");delivery.orders=decode_list(&list,"orders")?;
  for order in &delivery.orders{validate_transfer_order(order)?;ensure!(match order.role.as_str(){"sender"=>order.source_peer==target,"receiver"=>order.receiver_peer==target,"provider"=>order.provider_peer==target,_=>false},"订单不属于当前设备角色");self.validate_quote(order).await?;}
  let providers=client.read(ReadRequest::Delivery(DeliveryRead::Providers)).await?;delivery.providers=decode_list(&providers,"providers")?;self.state.delivery=delivery;self.publish();Ok(())
 }
 pub(super) async fn delivery_intent(&self,terms:Value)->Result<String>{
  let client=self.client()?;let terms:TransferTerms=serde_json::from_value(terms)?;let target=self.active_entry.as_ref().context("当前设备未确认")?.node_id.clone();let path=self.client_directory()?.join("delivery-intents.json");
  let fingerprint=format!("{:x}",Sha256::digest(serde_json::to_vec(&json!({"network":client.network_id(),"client":client.client_id(),"target":target,"terms":terms}))?));let mut entries=read_intents(path.clone()).await?;
  if let Some(intent)=entries.get(&fingerprint){return Ok(intent.intent_id.clone())}let intent_id=uuid::Uuid::new_v4().to_string();entries.insert(fingerprint,TransferIntent{intent_id:intent_id.clone(),network_id:client.network_id().into(),client_id:client.client_id().into(),target_node:target,terms});
  tokio::task::spawn_blocking(move||vibeface_bridge::platform::privatefs::write_atomic(&path,&serde_json::to_vec(&entries)?)).await??;Ok(intent_id)
 }
 pub(super) async fn validate_quote(&self,order:&TransferOrder)->Result<()>{
  if order.role!="sender"{return Ok(())}let client=self.client()?;let entries=read_intents(self.client_directory()?.join("delivery-intents.json")).await?;let Some(intent)=entries.values().find(|intent|intent.intent_id==order.intent_id)else{return Ok(())};let terms=&intent.terms;
  ensure!(intent.network_id==client.network_id()&&intent.client_id==client.client_id()&&intent.target_node==order.source_peer,"订单与原本机付款意图身份不一致");
  ensure!(order.receiver_peer==terms.receiver&&order.provider_peer==terms.provider&&order.mode==terms.mode&&order.acceptance==terms.acceptance&&order.name.as_deref()==Some(terms.name.as_str())&&order.bytes==terms.bytes&&order.sha256.as_deref()==Some(terms.sha256.as_str()),"订单与原文件或条款不一致，尚未确认付款");
  ensure!(amount_at_most(&order.payment.total,&terms.service_cap)&&amount_at_most(&order.payment.gas_cap,&terms.gas_cap),"订单费用超过原最高金额，尚未确认付款");Ok(())
 }
}
async fn read_intents(path:PathBuf)->Result<BTreeMap<String,TransferIntent>>{tokio::task::spawn_blocking(move||{if !path.try_exists()?{return Ok(BTreeMap::new())}let bytes=vibeface_bridge::platform::privatefs::read_private_bounded(&path,4*1024*1024,"文件传输意图")?;Ok(serde_json::from_slice(&bytes)?)}).await?}
fn amount_at_most(amount:&str,cap:&str)->bool{if amount.is_empty()||cap.is_empty()||!amount.bytes().chain(cap.bytes()).all(|b|b.is_ascii_digit()){return false}let amount=amount.trim_start_matches('0');let cap=cap.trim_start_matches('0');amount.len()<cap.len()||amount.len()==cap.len()&&amount<=cap}
