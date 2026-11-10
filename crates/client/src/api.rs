use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{collections::BTreeMap, path::PathBuf, sync::Arc};
use tokio::sync::{broadcast, Semaphore};
use std::sync::Mutex;
use crate::{ClientKey, Connection, Endpoint, wire::*, outbox::{Operation, Outbox, Reconciliation}};
pub use crate::outbox::{OperationSnapshot, OperationState};

#[derive(Clone, Debug)]
pub enum ReadRequest {
    Home, Conversation { conversation_id: String }, Workspace, Nodes, NodeSettings,
    Logs, Diagnostics, Questions, Notifications, RelaySettings, TriageSettings,
    ProviderModels { provider: Provider }, ProviderProbe { provider: Provider },
    Memory { conversation_id: String, memory_id: String },
    Work { conversation_id: String, work_id: String },
    Context { conversation_id: String, target: InputTarget },
    Artifact { conversation_id: String, blob_hash: String, offset: u64, limit: u64 },
    Delivery(DeliveryRead),
}
#[derive(Clone, Debug)]
pub enum DeliveryRead { Status, List, Providers, Get { order_id: String }, FindIntent { intent_id: String }, Read { order_id: String } }
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all="snake_case")]
pub enum Provider { Codex, Claude, Cursor, Opencode, Pi }
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MemoryVersion { pub revision: String, pub control_revision: u32, pub source_event_hash: String }
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MemoryBase { pub epoch: u32, pub version: Option<MemoryVersion> }
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BotProject { pub target_node: String, pub cwd: String }
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ManagerSettings { pub provider: Provider, pub workspace: PathBuf, pub model: String, pub reasoning_effort: Option<String>, #[serde(default)] pub backup: BackupPolicy }
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct BackupPolicy { pub automatic: bool, pub candidates: Vec<BackupModel> }
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BackupModel { pub provider: Provider, pub model: String, pub reasoning_effort: Option<String> }
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all="snake_case")]
pub enum AppServerMode { Spawn, System }
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AppServer { pub mode: AppServerMode, pub socket: Option<PathBuf> }
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all="snake_case")]
pub enum Outlet { Record, Both }
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ProjectOutlet { pub prefix: String, pub outlet: Outlet }
#[derive(Clone)]
pub enum SecretChange { Keep, Set(String), Clear }
#[derive(Clone)]
pub struct TriageSettings { pub default_outlet: Outlet, pub projects: Vec<ProjectOutlet>, pub webhook_url: Option<String>, pub scan_codex_sessions: bool, pub codex_sessions: Option<PathBuf>, pub webhook_secret: SecretChange }
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all="snake_case")]
pub enum Outline { Round, Squircle, Drop, Bean, Cloud }
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all="snake_case")]
pub enum Eyes { Dot, Pill, Oval, Wide }
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Avatar { pub outline: Outline, pub eyes: Eyes, pub hue: u16 }
/// A secret returned only to the explicit credential-copy flow, never a read or operation snapshot.
pub struct DecisionToken { pub token_id: String, token: zeroize::Zeroizing<String> }
impl DecisionToken { pub fn expose(&self)->&str { self.token.as_str() } }
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct NodeSettings { pub roots: Vec<PathBuf>, pub manager: Option<ManagerSettings> }
#[derive(Clone, Serialize, Deserialize)]
pub struct ProviderSettings {
    pub provider: Provider,
    #[serde(skip_serializing_if="Option::is_none")] pub bot: Option<bool>,
    #[serde(skip_serializing_if="Option::is_none")] pub execution: Option<bool>,
    #[serde(skip_serializing_if="Option::is_none")] pub execution_model: Option<String>,
    #[serde(skip_serializing_if="Option::is_none")] pub bin: Option<PathBuf>,
    #[serde(skip_serializing_if="Option::is_none")] pub app_server: Option<AppServer>,
    pub connect_local: bool,
    pub env: BTreeMap<String, Option<String>>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all="snake_case")]
pub enum DeliveryMode { Charity, Paid }
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all="snake_case")]
pub enum DeliveryAcceptance { Recoverable, Strict }
#[derive(Clone, Serialize, Deserialize)]
pub struct DeliveryPrepare {
    pub intent_id: String, pub receiver_peer: String, pub provider_peer: String,
    pub mode: DeliveryMode, pub acceptance: DeliveryAcceptance, pub name: String,
    pub content_base64: String, pub max_service_amount: String, pub max_gas_amount: String,
}
#[derive(Clone)]
pub enum DeliveryAction {
    Configure { enabled: bool, charity_enabled: bool, paid_enabled: bool },
    Prepare(DeliveryPrepare), Confirm { intent_id: String }, Accept { order_id: String }, Reconcile { order_id: String },
}
#[derive(Clone)]
pub enum Action {
    Input { conversation_id: String, input: InputRequest },
    Workspace { command: WorkspaceCommand },
    WorkspaceWithAttachments { command: WorkspaceCommand, conversation_id: String },
    Memory { conversation_id: String, memory_id: String, base: MemoryBase, after: Option<String> },
    Schedule { conversation_id: String, schedule_id: String, action: ScheduleAction },
    StopReply { conversation_id: String, trigger: String },
    StopWork { conversation_id: String, work_id: String },
    Answer { conversation_id: String, question_id: String, text: String },
    CreateBot { bot_id: String, conversation_id: String, name: String, instructions: String, scheduler_node: String, project: Option<BotProject> },
    DeleteBot { conversation_id: String },
    SetPrimaryBot { conversation_id: String },
    SetBotProject { conversation_id: String, project: Option<BotProject> },
    SetBotAvatar {conversation_id:String,version:Option<String>,avatar:Option<Avatar>},
    NodeSettings(NodeSettings), ProviderSettings(ProviderSettings), Triage(TriageSettings),
    Relay { volunteer: bool, listen: Option<String>, url: Option<String>, rate: Option<std::num::NonZeroU32>, max_clients: Option<std::num::NonZeroUsize>, max_clients_per_ip: Option<std::num::NonZeroUsize> },
    CreateDecisionToken {label:String}, RevokeDecisionToken {token_id:String},
    TestWebhook,
    Delivery(DeliveryAction),
}
#[derive(Clone, Debug)]
pub struct ClientEvent { pub generation: String, pub network_id: String, pub payload: Value }

#[derive(Clone)]
pub struct Client {
    key: Arc<ClientKey>, connection: Connection, target: String,
    outbox: Arc<Mutex<Outbox>>, events: broadcast::Sender<ClientEvent>,
    generation: String,
    writes: Arc<Semaphore>,
    decision_tokens: Arc<Mutex<BTreeMap<String,DecisionToken>>>,
}

impl Client {
    pub async fn open(key: Arc<ClientKey>, endpoint: Endpoint, outbox_dir: PathBuf) -> Result<Self> {
        let network = key.network_id().context("client is not enrolled")?.to_owned();
        let connection = Connection::open_endpoint(&endpoint, &key.identity).await?;
        connection.call("authenticate", json!({})).await?;
        let mut directory = connection.call("directory", json!({})).await?;
        let state: vibeface_secure::membership::NetworkState = serde_json::from_value(directory["state"].take())?;
        ensure!(state.genesis.network_id == network, "Node answered for another network");
        let now = directory["now_ms"].as_u64().context("directory time missing")?;
        let view = state.project_at(now)?;
        view.client(key.device_id())?;
        let node = view.member(endpoint.node_id())?;
        ensure!(node.kind == vibeface_secure::membership::IdentityKind::Node, "endpoint is not a network Node");
        let client_id = key.device_id().to_owned();
        let outbox_network = network.clone();
        let outbox = tokio::task::spawn_blocking(move || Outbox::open(&outbox_dir, &outbox_network, &client_id)).await??;
        let (events, _) = broadcast::channel(128);
        let generation = uuid::Uuid::new_v4().to_string();
        let mut pushes = connection.subscribe();
        let updates = events.clone();
        let token = generation.clone();
        tokio::spawn(async move {
            loop {
                let payload = match pushes.recv().await {
                    Ok(value) => value,
                    Err(broadcast::error::RecvError::Lagged(_)) => json!({"type":"resync_required"}),
                    Err(broadcast::error::RecvError::Closed) => { let _=updates.send(ClientEvent {generation:token.clone(),network_id:network.clone(),payload:json!({"type":"disconnected"})}); break; }
                };
                let _ = updates.send(ClientEvent { generation:token.clone(), network_id:network.clone(), payload });
            }
        });
        connection.call("subscribe",json!({})).await?;
        Ok(Self { key, connection, target:endpoint.node_id().into(), outbox:Arc::new(Mutex::new(outbox)), events, generation, writes:Arc::new(Semaphore::new(8)), decision_tokens:Arc::new(Mutex::new(BTreeMap::new())) })
    }
    pub fn take_decision_token(&self, operation_id:&str)->Result<Option<DecisionToken>> {
        Ok(self.decision_tokens.lock().map_err(|_|anyhow::anyhow!("token lock poisoned"))?.remove(operation_id))
    }
    pub fn generation(&self) -> &str { &self.generation }
    pub fn network_id(&self) -> &str { self.key.network_id().expect("validated at open") }
    pub fn client_id(&self) -> &str { self.key.device_id() }
    pub fn subscribe(&self) -> broadcast::Receiver<ClientEvent> { self.events.subscribe() }
    async fn stored<T: Send + 'static>(&self, task: impl FnOnce(&mut Outbox)->Result<T> + Send + 'static) -> Result<T> {
        let outbox=self.outbox.clone();
        tokio::task::spawn_blocking(move || { let mut store=outbox.lock().map_err(|_|anyhow::anyhow!("outbox lock poisoned"))?; task(&mut store) }).await?
    }
    pub async fn next_input_sequence(&self, conversation_id: &str, target: &InputTarget) -> Result<u64> {
        let lane=format!("input:{}:{}",conversation_id,serde_json::to_string(target)?);
        self.stored(move|db|db.reserve_sequence(&lane)).await
    }
    pub async fn pending(&self) -> Result<Vec<OperationSnapshot>> { self.stored(|db|db.snapshots()).await }
    pub async fn store_attachment(&self, name: String, data: Vec<u8>) -> Result<Attachment> { self.stored(move|db|db.store_attachment(&name,&data)).await }
    pub async fn read(&self, request: ReadRequest) -> Result<Value> {
        let (rpc, method, mut params) = request.wire();
        if rpc { params["method"]=json!(method); self.connection.call("rpc",params).await }
        else { self.connection.call(method,params).await }
    }
    /// The worker owns the operation after queueing; cancelling the UI future cannot interrupt persistence/send/settlement.
    pub async fn submit(&self, action: Action) -> Result<OperationSnapshot> {
        let permit=self.writes.clone().acquire_owned().await?;
        let client=self.clone();
        tokio::spawn(async move { let _permit=permit; client.submit_owned(action).await }).await?
    }
    async fn submit_owned(&self, action: Action) -> Result<OperationSnapshot> {
        let operation=self.prepare(action)?;
        let id=operation.snapshot.id.clone();
        self.stored(move|db|db.prepare(&operation)).await?;
        let lookup=id.clone();
        let existing=self.stored(move|db|db.get(&lookup)).await?;
        if existing.snapshot.state!=OperationState::Prepared { return Ok(existing.snapshot); }
        let operation=self.stored(move|db|db.begin(&id)).await?;
        let id=operation.snapshot.id.clone();
        let sent = async {
            if let Some(conversation_id)=&operation.upload_scope {
                for attachment in &operation.attachments { self.upload(conversation_id,attachment).await?; }
            }
            self.connection.call_bytes(operation.bytes.clone()).await
        }.await;
        match sent {
            Ok(mut result) => {
                let request:Value=serde_json::from_slice(&operation.bytes)?;
                if request["method"]=="create_decision_token" {
                    let token=result.as_object_mut().and_then(|o|o.remove("token")).context("created token missing")?;
                    let token=zeroize::Zeroizing::new(token.as_str().context("invalid token")?.to_owned());
                    let token_id=result["token_id"].as_str().context("token ID missing")?.to_owned();
                    let mut secrets=self.decision_tokens.lock().map_err(|_|anyhow::anyhow!("token lock poisoned"))?;
                    ensure!(secrets.len()<16,"copy or discard previously created tokens first");
                    secrets.insert(id.clone(),DecisionToken {token_id,token});
                }
                if let Some(state)=validate_result(&operation,&result)? {
                    self.stored(move|db|db.settle(&id,state,result)).await
                } else { Ok(operation.snapshot) }
            }
            // Generic RPC errors do not prove no side effect; only method-specific receipts settle a write.
            Err(_) => Ok(operation.snapshot),
        }
    }
    pub async fn reconcile(&self, id: &str) -> Result<OperationSnapshot> {
        let id=id.to_owned();
        let op=self.stored(move|db|db.get(&id)).await?;
        ensure!(op.snapshot.target_node==self.target && op.snapshot.network_id==self.network_id() && op.snapshot.client_id==self.client_id(),"operation belongs to another endpoint or identity");
        if op.snapshot.state!=OperationState::Unknown { return Ok(op.snapshot); }
        let result=match &op.reconciliation {
            Reconciliation::Input {conversation_id,message_id,target} => self.connection.call("rpc",json!({"method":"input_receipt","conversation_id":conversation_id,"message_id":message_id,"target":target})).await?,
            Reconciliation::Memory {conversation_id,memory_id} => self.connection.call("rpc",json!({"method":"memory_state","conversation_id":conversation_id,"memory_id":memory_id,"operation_id":op.snapshot.id})).await?,
            Reconciliation::Signed {scope_id,body,attestation} => self.connection.call("rpc",json!({"method":"signed_receipt","scope_id":scope_id,"body":body,"attestation":attestation})).await?,
            Reconciliation::Workspace => {
                let mut request:Value=serde_json::from_slice(&op.bytes)?;
                let mut params=request["params"].take();params["method"]=json!("workspace_command_receipt");
                self.connection.call("rpc",params).await?
            }
            Reconciliation::Delivery {intent_id,order_id,..} => {
                let order_id=if let Some(id)=order_id {Some(id.clone())} else if let Some(intent)=intent_id {
                    let found=self.connection.call("delivery",json!({"op":"find_intent","intent_id":intent})).await?;
                    if found["order"].is_null() {None} else {Some(found["order"]["order_id"].as_str().context("delivery order ID missing")?.to_owned())}
                } else {None};
                if let Some(id)=order_id {self.connection.call("delivery",json!({"op":"receipt","order_id":id})).await?} else {Value::Null}
            }
            Reconciliation::Manual => return Ok(op.snapshot),
        };
        let state=match &op.reconciliation {
            Reconciliation::Input {message_id,target,..} => {
                ensure!(result["message_id"]==*message_id && result["target"]==serde_json::to_value(target)?,"receipt identifies another input");
                match result["stage"].as_str() {
                    Some("entry_accepted"|"owner_accepted"|"submitting"|"provider_accepted"|"associated"|"refused") => Some(OperationState::Accepted),
                    Some("unknown") => None,
                    _ => anyhow::bail!("invalid input receipt stage"),
                }
            }
            Reconciliation::Memory {memory_id,..} => {
                match result["receipt"].as_str() {
                    Some("recorded") => { ensure!(result["current"]["memory_id"]==*memory_id && result["current"]["metadata"]["revision"]==op.snapshot.id,"memory receipt identity mismatch"); Some(OperationState::Accepted) },
                    Some("superseded") => Some(OperationState::Rejected),
                    Some("not_seen_here") => None,
                    _ => anyhow::bail!("invalid memory receipt"),
                }
            }
            Reconciliation::Signed {scope_id,..} => {
                ensure!(result["network_id"]==op.snapshot.network_id && result["client_id"]==op.snapshot.client_id && result["scope_id"]==*scope_id,"signed receipt identity mismatch");
                match result["receipt"].as_str() {Some("recorded")=>{ensure!(valid_hash(&result["event_hash"]),"missing signed event evidence");Some(OperationState::Accepted)},Some("not_seen_here")=>None,_=>anyhow::bail!("invalid signed receipt")}
            }
            Reconciliation::Workspace => {
                ensure!(result["network_id"]==op.snapshot.network_id && result["client_id"]==op.snapshot.client_id && result["command_id"]==op.snapshot.id,"workspace receipt identity mismatch");
                match result["receipt"].as_str() { Some("recorded")=> { ensure!(valid_hash(&result["event_hash"]),"workspace receipt has no event evidence");Some(OperationState::Accepted) }, Some("not_seen_here")=>None,_=>anyhow::bail!("invalid workspace receipt") }
            }
            Reconciliation::Delivery {intent_id,order_id,operation} => {
                if result.is_null() { None } else {
                    let receipt=&result["receipt"];
                    if receipt.is_null() { return Ok(op.snapshot); }
                    ensure!(receipt["network_id"]==op.snapshot.network_id && receipt["node_id"]==op.snapshot.target_node,"delivery receipt identity mismatch");
                    if let Some(id)=intent_id {ensure!(receipt["intent_id"]==*id,"delivery intent mismatch");}
                    if let Some(id)=order_id {ensure!(receipt["order_id"]==*id,"delivery order mismatch");}
                    let request:Value=serde_json::from_slice(&op.bytes)?;
                    if operation=="prepare" { validate_delivery_prepare(&request["params"],&result["order"])?; }
                    match operation.as_str() {
                        "prepare"=>Some(OperationState::Accepted),
                        "confirm" if receipt["confirmed"]==true && receipt["authorized_by"]==op.snapshot.client_id=>Some(OperationState::Accepted),
                        "accept" if receipt["accepted"]==true && receipt["authorized_by"]==op.snapshot.client_id=>Some(OperationState::Accepted),
                        _=>None,
                    }
                }
            }
            Reconciliation::Manual=>None,
        };
        if let Some(state)=state { let id=op.snapshot.id; self.stored(move|db|db.settle(&id,state,result)).await } else { Ok(op.snapshot) }
    }
    async fn upload(&self, conversation: &str, attachment: &Attachment) -> Result<()> {
        use base64::Engine;
        let reference=attachment.clone();
        let data=self.stored(move|db|db.read_attachment(&reference)).await?;
        let mut offset=0usize;
        let mut attempts=0usize;
        while offset < data.len() {
            attempts+=1; ensure!(attempts<=data.len()/65536+3,"attachment upload did not advance");
            let end=(offset+65536).min(data.len());
            let result=self.connection.call("rpc",json!({"method":"upload_attachment","conversation_id":conversation,"hash":attachment.hash,"size":attachment.size,"offset":offset,"data":base64::engine::general_purpose::STANDARD.encode(&data[offset..end])})).await?;
            let next=result["offset"].as_u64().context("upload offset missing")?;
            ensure!(next<=data.len() as u64,"upload offset exceeds content");
            offset=next as usize;
            if result["complete"]==true { ensure!(offset==data.len(),"incomplete attachment receipt"); break; }
        }
        Ok(())
    }
    fn attest(&self, scope: &str, body: &Value) -> Result<Value> {
        let payload=vibeface_secure::join::attestation_payload(self.network_id(),scope,&body.to_string())?;
        Ok(json!({"device_id":self.client_id(),"signature":self.key.sign(vibeface_secure::join::ATTESTATION_DOMAIN,&payload)?}))
    }
    fn operation_id(&self, scope: &str, body: &Value) -> Result<String> {
        let mut bytes=vibeface_secure::join::attestation_payload(self.network_id(),scope,&body.to_string())?;
        bytes.extend_from_slice(self.client_id().as_bytes()); Ok(hex::encode(Sha256::digest(bytes)))
    }
    fn prepare(&self, action: Action) -> Result<Operation> {
        let upload_scope=match &action {
            Action::Input {conversation_id,..}|Action::WorkspaceWithAttachments {conversation_id,..}=>Some(conversation_id.clone()),_=>None,
        };
        let mut id=uuid::Uuid::new_v4().to_string();
        let mut sequence=None;
        let mut revision=None;
        let mut attachments=vec![];
        let mut reconciliation=Reconciliation::Manual;
        let (rpc,method,mut params,lane,signature)=match action {
            Action::Input {conversation_id,input} => {
                ensure!(input.route.sequence>0,"input sequence must be positive");
                id=input.message_id.clone(); sequence=Some(input.route.sequence); revision=input.route.expected_revision;
                attachments=input.attachments.clone();
                reconciliation=Reconciliation::Input {conversation_id:conversation_id.clone(),message_id:id.clone(),target:input.route.target.clone()};
                let mut body=serde_json::to_value(&input)?; body["type"]=json!("input_posted");
                let mut params=serde_json::to_value(&input)?; params["conversation_id"]=json!(conversation_id);
                let lane=format!("input:{}:{}",conversation_id,serde_json::to_string(&input.route.target)?);
                (true,"message",params,lane,Some((conversation_id,body)))
            }
            Action::Workspace {command} | Action::WorkspaceWithAttachments {command,..} => {
                attachments=match &command {WorkspaceCommand::PostChannelMessage {attachments,..}|WorkspaceCommand::ControlInput {action:InputControl::Edit {attachments,..},..}=>attachments.clone(),_=>vec![]};
                ensure!(attachments.is_empty() || upload_scope.is_some(),"workspace attachments require an upload conversation");
                reconciliation=Reconciliation::Workspace;
                id=self.operation_id("workspace",&json!({"type":"workspace_commanded","command":command}))?;
                revision=serde_json::to_value(&command)?["expected_revision"].as_u64();
                let body=json!({"type":"workspace_commanded","command_id":id,"command":command});
                (true,"workspace_command",json!({"command_id":id,"command":command}),"workspace".into(),Some(("workspace".into(),body)))
            }
            Action::Memory {conversation_id,memory_id,base,after} => {
                ensure!(base.epoch==1,"unsupported memory epoch");
                let body=json!({"type":"memory_controlled","memory_id":memory_id,"base":base,"after":after});
                id=self.operation_id(&conversation_id,&body)?;
                reconciliation=Reconciliation::Memory {conversation_id:conversation_id.clone(),memory_id:memory_id.clone()};
                (true,"control_memory",json!({"conversation_id":conversation_id,"memory_id":memory_id,"base":base,"after":after,"operation_id":id}),format!("memory:{conversation_id}:{memory_id}"),Some((conversation_id,body)))
            }
            Action::Schedule {conversation_id,schedule_id,action} => {
                let body=json!({"type":"schedule_controlled","schedule_id":schedule_id,"action":action});
                (true,"control_schedule",json!({"conversation_id":conversation_id,"schedule_id":schedule_id,"action":action}),format!("schedule:{conversation_id}:{schedule_id}"),Some((conversation_id,body)))
            }
            Action::StopReply {conversation_id,trigger} => (true,"cancel_manager",json!({"conversation_id":conversation_id,"trigger":trigger}),format!("reply:{conversation_id}:{trigger}"),None),
            Action::StopWork {conversation_id,work_id} => (true,"interrupt_work",json!({"conversation_id":conversation_id,"work_id":work_id}),format!("work:{conversation_id}:{work_id}"),None),
            Action::Answer {conversation_id,question_id,text} => {
                id=format!("a-{question_id}");
                let body=json!({"type":"message_posted","message_id":id,"role":"user","text":text,"report_results":true,"answers":question_id});
                (true,"answer",json!({"question_id":question_id,"text":text}),format!("answer:{question_id}"),Some((conversation_id,body)))
            }
            Action::CreateBot {bot_id,conversation_id,name,instructions,scheduler_node,project} => {
                let mut body=json!({"type":"bot_created","bot_id":bot_id,"name":name,"instructions":instructions});
                let mut params=json!({"bot_id":bot_id,"conversation_id":conversation_id,"name":name,"instructions":instructions,"scheduler_node":scheduler_node,
                    "owner_attestation":self.attest(&conversation_id,&json!({"type":"conversation_owner","scheduler_node":scheduler_node,"epoch":1}))?});
                if let Some(project)=project { body["project"]=serde_json::to_value(&project)?;params["project"]=serde_json::to_value(project)?; }
                (true,"create_bot",params,format!("bot:{conversation_id}"),Some((conversation_id,body)))
            }
            Action::DeleteBot {conversation_id} => (true,"delete_bot",json!({"conversation_id":conversation_id}),format!("bot:{conversation_id}"),Some((conversation_id,json!({"type":"bot_deleted"})))),
            Action::SetPrimaryBot {conversation_id} => (true,"set_primary_bot",json!({"conversation_id":conversation_id}),"primary".into(),Some(("bots".into(),json!({"type":"primary_bot_set","conversation_id":conversation_id})))),
            Action::SetBotProject {conversation_id,project} => {
                let body=json!({"type":"bot_project_set","project":project});
                (true,"set_bot_project",json!({"conversation_id":conversation_id,"project":project}),format!("bot:{conversation_id}"),Some((conversation_id,body)))
            }
            Action::SetBotAvatar {conversation_id,version,avatar} => {
                if let Some(avatar)=&avatar { ensure!([20,45,100,130,155,180,205,230,255,285,315,345].contains(&avatar.hue),"invalid avatar hue"); }
                let body=json!({"type":"bot_avatar_set","version":version,"avatar":avatar});
                (true,"set_bot_avatar",json!({"conversation_id":conversation_id,"version":version,"avatar":avatar}),format!("bot:{conversation_id}"),Some((conversation_id,body)))
            }
            Action::NodeSettings(settings) => (false,"configure_node",serde_json::to_value(settings)?,"node-settings".into(),None),
            Action::ProviderSettings(settings) => (false,"configure_provider",serde_json::to_value(settings)?,"provider-settings".into(),None),
            Action::Relay {volunteer,listen,url,rate,max_clients,max_clients_per_ip} => (false,"configure_relay",json!({"volunteer":volunteer,"listen":listen,"url":url,"rate":rate,"max_clients":max_clients,"max_clients_per_ip":max_clients_per_ip}),"relay".into(),None),
            Action::Triage(settings) => {
                let mut value=json!({"default_outlet":settings.default_outlet,"projects":settings.projects,"webhook_url":settings.webhook_url,"scan_codex_sessions":settings.scan_codex_sessions,"codex_sessions":settings.codex_sessions});
                match settings.webhook_secret { SecretChange::Keep=>{},SecretChange::Clear=>value["webhook_secret"]=Value::Null,SecretChange::Set(secret)=>value["webhook_secret"]=json!(secret) }
                (false,"configure_triage",value,"triage".into(),None)
            }
            Action::CreateDecisionToken {label} => (false,"create_decision_token",json!({"label":label}),"decision-token".into(),None),
            Action::RevokeDecisionToken {token_id} => (false,"revoke_decision_token",json!({"token_id":token_id}),"decision-token".into(),None),
            Action::TestWebhook => (false,"test_webhook",json!({}),"webhook-test".into(),None),
            Action::Delivery(action) => {
                let params=match action {
                    DeliveryAction::Configure {enabled,charity_enabled,paid_enabled} => json!({"op":"configure","enabled":enabled,"charity_enabled":charity_enabled,"paid_enabled":paid_enabled}),
                    DeliveryAction::Prepare(prepare) => { id=format!("prepare-{}",prepare.intent_id); let mut value=serde_json::to_value(prepare)?;value["op"]=json!("prepare");value },
                    DeliveryAction::Confirm {intent_id} => { id=format!("confirm-{intent_id}");json!({"op":"confirm","intent_id":intent_id}) },
                    DeliveryAction::Accept {order_id} => { id=format!("accept-{order_id}");json!({"op":"accept","order_id":order_id}) },
                    DeliveryAction::Reconcile {order_id} => json!({"op":"reconcile","order_id":order_id}),
                };
                if params["op"]!="configure" {
                    reconciliation=Reconciliation::Delivery {intent_id:params["intent_id"].as_str().map(str::to_owned),order_id:params["order_id"].as_str().map(str::to_owned),operation:params["op"].as_str().unwrap_or_default().into()};
                }
                (false,"delivery",params,"delivery".into(),None)
            }
        };
        if let Some((scope,body))=signature {
            params["attestation"]=self.attest(&scope,&body)?;
            if matches!(reconciliation,Reconciliation::Manual) {reconciliation=Reconciliation::Signed {scope_id:scope,body,attestation:params["attestation"].clone()};}
        }
        let method=if rpc { params["method"]=json!(method); "rpc" } else {method};
        let bytes=serde_json::to_vec(&json!({"id":id,"method":method,"params":params}))?;
        Ok(Operation {snapshot:OperationSnapshot {id,network_id:self.network_id().into(),client_id:self.client_id().into(),target_node:self.target.clone(),state:OperationState::Prepared,sequence,result:None},lane,expected_revision:revision,bytes,reconciliation,attachments,upload_scope})
    }
}

fn validate_result(op: &Operation, result: &Value) -> Result<Option<OperationState>> {
    let request: Value=serde_json::from_slice(&op.bytes)?;
    if request["method"]=="delivery" {
        match request["params"]["op"].as_str() {
            Some("prepare")=>validate_delivery_prepare(&request["params"],result)?,
            Some("confirm")=>ensure!(result["intent_id"]==request["params"]["intent_id"],"delivery confirmation identity mismatch"),
            Some("accept"|"reconcile")=>ensure!(result["order_id"]==request["params"]["order_id"],"delivery receipt identity mismatch"),
            _=>{},
        }
    }
    let method=request["params"]["method"].as_str().unwrap_or_default();
    match method {
        "message" => {
            ensure!(result["message_id"]==op.snapshot.id,"input response has another message ID");
            if result["accepted"]==true { Ok(Some(OperationState::Accepted)) }
            else if result["stored"]==false && result["code"]=="input_not_saved" { Ok(Some(OperationState::Rejected)) }
            else { Ok(None) }
        }
        "workspace_command" => {
            ensure!(result["command_id"]==op.snapshot.id,"workspace response has another command ID");
            if result["stored"]==false && result["code"]=="workspace_command_not_saved" { Ok(Some(OperationState::Rejected)) }
            else if result["event_hash"].as_str().is_some_and(|v|v.len()==64) { Ok(Some(OperationState::Accepted)) }
            else { Ok(None) }
        }
        "control_memory" => {
            ensure!(result["operation_id"]==op.snapshot.id,"memory response has another operation ID");
            match result["state"].as_str() {
                Some("applied"|"already_recorded")=>Ok(Some(OperationState::Accepted)),
                Some("superseded"|"conflict")=>Ok(Some(OperationState::Rejected)),
                _=>Ok(None),
            }
        }
        _=> { ensure!(result.is_object(),"mutation result must be an object"); Ok(Some(OperationState::Accepted)) }
    }
}

impl ReadRequest {
    fn wire(self)->(bool,&'static str,Value) {
        match self {
            Self::Home=>(true,"bots",json!({})),
            Self::Conversation {conversation_id}=>(true,"conversation",json!({"conversation_id":conversation_id})),
            Self::Workspace=>(true,"workspace_state",json!({})),
            Self::Nodes=>(true,"nodes",json!({})),
            Self::NodeSettings=>(false,"node_settings",json!({})),
            Self::Logs=>(false,"logs",json!({})),
            Self::Diagnostics=>(false,"diagnostics",json!({})),
            Self::Questions=>(true,"questions",json!({})),
            Self::Notifications=>(true,"notifications",json!({})),
            Self::RelaySettings=>(false,"relay_settings",json!({})),
            Self::TriageSettings=>(false,"triage_settings",json!({})),
            Self::ProviderModels {provider}=>(false,"provider_models",json!({"provider":provider})),
            Self::ProviderProbe {provider}=>(false,"provider_probe",json!({"provider":provider})),
            Self::Memory {conversation_id,memory_id}=>(true,"memory_state",json!({"conversation_id":conversation_id,"memory_id":memory_id})),
            Self::Work {conversation_id,work_id}=>(true,"work_detail",json!({"conversation_id":conversation_id,"work_id":work_id})),
            Self::Context {conversation_id,target}=>(true,"scope_context",json!({"conversation_id":conversation_id,"target":target})),
            Self::Artifact {conversation_id,blob_hash,offset,limit}=>(true,"read_artifact",json!({"conversation_id":conversation_id,"blob_hash":blob_hash,"offset":offset,"limit":limit})),
            Self::Delivery(read)=>(false,"delivery",match read {
                DeliveryRead::Status=>json!({"op":"status"}), DeliveryRead::List=>json!({"op":"list"}),DeliveryRead::Providers=>json!({"op":"providers"}),
                DeliveryRead::Get {order_id}=>json!({"op":"get","order_id":order_id}),DeliveryRead::FindIntent {intent_id}=>json!({"op":"find_intent","intent_id":intent_id}),DeliveryRead::Read {order_id}=>json!({"op":"read","order_id":order_id}),
            }),
        }
    }
}

fn valid_hash(value:&Value)->bool { value.as_str().is_some_and(|s|s.len()==64 && s.bytes().all(|c|matches!(c,b'0'..=b'9'|b'a'..=b'f'))) }

#[cfg(test)]
mod tests {
    use super::*;
    use crate::connection::tests::{receive,send,server};
    use vibeface_secure::{Identity,membership::{Genesis,Member,NetworkState}};
    use std::sync::atomic::{AtomicUsize,Ordering};
    use futures_util::SinkExt;

    #[tokio::test]
    async fn lost_receipt_and_restart_query_the_original_operation_without_duplicate_effect() -> Result<()> {
        let dir=std::env::temp_dir().join(format!("native-restart-{}",uuid::Uuid::new_v4()));
        let mut key=ClientKey::create(&dir.join("key"),"controlled fixture password")?;
        let node=Identity::generate()?;
        let node_secret=node.secret_hex();
        let genesis=Genesis::create(&Identity::generate()?,Member::client(&key.identity,"fixture",true)?,Member::node(&node,"fixture-node")?)?;
        let network=genesis.network_id.clone();let state=NetworkState::new(genesis)?;
        let effects=Arc::new(AtomicUsize::new(0));
        let input=InputRequest {message_id:"original-message".into(),route:InputRoute {target:InputTarget::BotMain {bot_id:"bot".into()},intent:InputIntent::Continue,sequence:1,expected_revision:None},text:"controlled input".into(),attachments:vec![],report_results:Some(true)};
        for cycle in 0..2 {
            let listener=tokio::net::TcpListener::bind("127.0.0.1:0").await?;
            let origin=format!("http://{}",listener.local_addr()?);
            let entry=crate::Entry {origin,node_id:node.device_id()};
            key.remember(&network,entry.clone())?;
            let fixture_node=Identity::from_secret_hex(&node_secret)?;let fixture_state=state.clone();let counter=effects.clone();let fixture_input=input.clone();
            let task=tokio::spawn(async move {
                let (mut socket,mut channel)=server(listener,&fixture_node).await?;
                loop {
                    let request=receive(&mut socket,&mut channel).await?;
                    let result=match request["method"].as_str() {
                        Some("authenticate"|"subscribe")=>json!({}),
                        Some("directory")=>json!({"state":fixture_state,"now_ms":vibeface_protocol::now_ms()}),
                        Some("rpc")=>match request["params"]["method"].as_str() {
                            Some("message")=> {counter.fetch_add(1,Ordering::SeqCst); socket.close(None).await?;break;},
                            Some("input_receipt")=>json!({"message_id":fixture_input.message_id,"target":fixture_input.route.target,"stage":"entry_accepted","retry_safe":false,"control_event":null,"actual_turn_id":null,"reason":null}),
                            _=>anyhow::bail!("unexpected fixture RPC"),
                        },
                        _=>anyhow::bail!("unexpected fixture method"),
                    };
                    send(&mut socket,&mut channel,json!({"id":request["id"],"result":result})).await?;
                    if request["params"]["method"]=="input_receipt" {break;}
                }
                Ok::<_,anyhow::Error>(())
            });
            let reopened=Arc::new(ClientKey::open(&dir.join("key"),"controlled fixture password")?);
            let client=Client::open(reopened,Endpoint::Gateway(entry),dir.join("outbox")).await?;
            let snapshot=client.submit(Action::Input {conversation_id:"chat".into(),input:input.clone()}).await?;
            assert_eq!(snapshot.state,OperationState::Unknown);
            if cycle==1 {
                let receipt=client.reconcile("original-message").await?;
                assert_eq!(receipt.state,OperationState::Accepted);
            }
            drop(client);task.await??;
        }
        assert_eq!(effects.load(Ordering::SeqCst),1);
        std::fs::remove_dir_all(dir)?;Ok(())
    }
    #[test]
    fn malformed_or_mismatched_receipts_never_settle_writes() -> Result<()> {
        let operation=Operation {snapshot:OperationSnapshot {id:"message-1".into(),network_id:"n".into(),client_id:"c".into(),target_node:"t".into(),state:OperationState::Unknown,sequence:Some(1),result:None},lane:"chat".into(),expected_revision:None,bytes:serde_json::to_vec(&json!({"params":{"method":"message"}}))?,reconciliation:Reconciliation::Manual,attachments:vec![],upload_scope:None};
        assert!(validate_result(&operation,&json!({"accepted":true,"message_id":"different"})).is_err());
        assert_eq!(validate_result(&operation,&json!({"accepted":false,"message_id":"message-1"}))?,None);
        assert_eq!(validate_result(&operation,&json!({"accepted":true,"message_id":"message-1"}))?,Some(OperationState::Accepted));
        Ok(())
    }
}

fn validate_delivery_prepare(request:&Value,order:&Value)->Result<()> {
    use base64::Engine;
    for field in ["intent_id","receiver_peer","provider_peer","mode","acceptance","name"] {
        ensure!(order[field]==request[field],"delivery preparation changed {field}");
    }
    let data=base64::engine::general_purpose::STANDARD.decode(request["content_base64"].as_str().context("delivery content missing")?)?;
    ensure!(order["bytes"].as_u64()==Some(data.len() as u64) && order["sha256"]==hex::encode(Sha256::digest(&data)),"delivery content mismatch");
    for (actual,maximum) in [("total","max_service_amount"),("gas_cap","max_gas_amount")] {
        let actual=order["payment"][actual].as_str().context("delivery amount missing")?;
        let maximum=request[maximum].as_str().context("delivery maximum missing")?;
        ensure!(decimal_at_most(actual,maximum),"delivery exceeds authorized budget");
    }
    Ok(())
}
fn decimal_at_most(actual:&str,maximum:&str)->bool {
    if actual.is_empty()||maximum.is_empty()||!actual.bytes().chain(maximum.bytes()).all(|b|b.is_ascii_digit()) {return false;}
    let actual=actual.trim_start_matches('0');let maximum=maximum.trim_start_matches('0');
    actual.len()<maximum.len() || actual.len()==maximum.len() && actual<=maximum
}
