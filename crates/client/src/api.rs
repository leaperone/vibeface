use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{collections::BTreeMap, path::PathBuf, sync::Arc};
use tokio::sync::{broadcast, Semaphore};
use std::sync::{Mutex, atomic::{AtomicBool, AtomicU8, Ordering}};
use crate::{ClientKey, Connection, Endpoint, wire::*, outbox::{Operation, Outbox, Reconciliation}};
pub use crate::outbox::{OperationSnapshot, OperationState};

#[derive(Clone, Debug)]
pub enum ReadRequest {
    ConversationHistory(vibeface_protocol::history::HistoryRequest),
    WriteState, Home, Conversation { conversation_id: String }, Workspace, WorkspaceOverview,
    WorkspaceHistory {channel_id:Option<String>,task_id:Option<String>,limit:usize}, Nodes, NodeSettings,
    Logs, LogsFiltered(LogFilter), Diagnostics, Questions, Notifications, RelaySettings, TriageSettings,
    ProviderModels { provider: Provider }, ProviderProbe { provider: Provider },
    Memory { conversation_id: String, memory_id: String },
    Work { conversation_id: String, work_id: String },
    Context { conversation_id: String, target: InputTarget },
    Artifact { conversation_id: String, blob_hash: String, offset: u64, limit: u64 },
    ArtifactBytes {conversation_id:String,blob_hash:String,offset:u64},
    Delivery(DeliveryRead),
}
#[derive(Clone,Debug,Serialize,Deserialize)]
#[serde(rename_all="lowercase")]
pub enum LogLevel {Trace,Debug,Info,Warn,Error}
#[derive(Clone,Debug,Serialize,Deserialize)]
#[serde(rename_all="lowercase")]
pub enum LogSource {Node,Web,Agent,Cli,Codex,Cursor}
#[derive(Clone,Debug,Default,Serialize,Deserialize)]
pub struct LogFilter {pub since_ms:Option<i64>,pub until_ms:Option<i64>,pub level:Option<LogLevel>,pub sources:Vec<LogSource>,pub conversation_id:Option<String>,pub work_id:Option<String>,pub text:Option<String>,pub limit:Option<usize>}
#[derive(Clone,Copy,Debug,PartialEq,Eq)]
pub enum ClientAuthority {Membership {can_manage:bool},LocalSession,PrivateDataOnly {reason:OfflineBlockReason}}
#[derive(Clone,Copy,Debug,PartialEq,Eq)]
pub enum OfflineBlockReason {Revoked,Forked,NotEnrolled,NotMember,NodeRemoved}
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
#[derive(Clone,Debug,Serialize,Deserialize)]
#[serde(rename_all="snake_case")]
pub enum ResolveOutcome {Completed,Interrupted}
#[derive(Clone)]
pub enum Action {
    Input { conversation_id: String, input: InputRequest },
    Workspace { command: WorkspaceCommand },
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
    TestWebhook, ConfirmWritable,
    AckNotification {conversation_id:String,notification_id:String},
    ResolveCheck {conversation_id:String,check_id:String,work_id:Option<String>,outcome:Option<ResolveOutcome>,note:String},
    Delivery(DeliveryAction),
}
#[derive(Clone, Debug)]
pub struct ClientEvent { pub generation: String, pub network_id: String, pub payload: Value }

#[derive(Debug)]
struct Revoked;
impl std::fmt::Display for Revoked {fn fmt(&self,f:&mut std::fmt::Formatter<'_>)->std::fmt::Result {f.write_str("client or target Node was revoked")}}
impl std::error::Error for Revoked {}
#[derive(Clone)]
pub struct Client {
    key: Arc<ClientKey>, connection: Connection, target: String,
    network:String,authority:ClientAuthority,endpoint:Endpoint,
    outbox: Arc<Mutex<Outbox>>, events: broadcast::Sender<ClientEvent>,
    generation: String,
    writes: Arc<Semaphore>,
    decision_tokens: Arc<Mutex<BTreeMap<String,DecisionToken>>>,
    active: Arc<AtomicBool>,
    revoked:Arc<AtomicBool>,write_gate:Arc<AtomicU8>,
}

impl Client {
    pub async fn open(key: Arc<ClientKey>, endpoint: Endpoint, outbox_dir: PathBuf) -> Result<Self> {
        let (connection,network,authority,proof)=match Self::authenticate(&key,&endpoint).await {Ok(value)=>value,Err(error)=>{if error.is::<Revoked>() {Self::invalidate_cached(&key,&endpoint,&outbox_dir).await?;}return Err(error);}};
        let client_id=key.device_id().to_owned();
        let outbox_network=network.clone();
        let outbox=tokio::task::spawn_blocking(move||Outbox::open(&outbox_dir,&outbox_network,&client_id)).await??;
        Self::attach(key,endpoint,connection,network,authority,proof,Arc::new(Mutex::new(outbox)),Arc::new(Semaphore::new(8)),Arc::new(Mutex::new(BTreeMap::new()))).await
    }
    async fn invalidate_cached(key:&ClientKey,endpoint:&Endpoint,outbox_dir:&std::path::Path)->Result<()> {
        if !outbox_dir.join("outbox.sqlite3").exists() {return Ok(());}
        let Some(network)=key.network_id() else {return Ok(());};
        let network=network.to_owned();let id=key.device_id().to_owned();let target=endpoint.node_id().to_owned();let dir=outbox_dir.to_owned();
        tokio::task::spawn_blocking(move||Outbox::open(&dir,&network,&id)?.invalidate_profile(&target)).await?
    }
    pub async fn open_or_offline(key:Arc<ClientKey>,endpoint:Endpoint,outbox_dir:PathBuf)->Result<Self> {
        match Self::open(key.clone(),endpoint.clone(),outbox_dir.clone()).await {
            Ok(client)=>Ok(client),
            Err(error) if crate::connection::transport_failure(&error)=>Self::open_offline(key,endpoint,outbox_dir).await.context("Node is unreachable and no usable verified offline profile is available"),
            Err(error)=>Err(error),
        }
    }
    pub async fn open_offline(key:Arc<ClientKey>,endpoint:Endpoint,outbox_dir:PathBuf)->Result<Self> {
        let known_network=key.network_id().map(str::to_owned);
        let client_id=key.device_id().to_owned();let cache_target=endpoint.node_id().to_owned();
        let (outbox,proof)=tokio::task::spawn_blocking(move||->Result<_>{let network=match known_network {Some(network)=>network,None=>Outbox::stored_network(&outbox_dir,&client_id)?};let db=Outbox::open(&outbox_dir,&network,&client_id)?;let proof=db.cached_profile(&cache_target)?;Ok((db,proof))}).await??;
        Self::offline_generation(key,endpoint,proof,Arc::new(Mutex::new(outbox)),Arc::new(Semaphore::new(8)),Arc::new(Mutex::new(BTreeMap::new())))
    }
    fn offline_generation(key:Arc<ClientKey>,endpoint:Endpoint,proof:crate::profile::CachedProfile,outbox:Arc<Mutex<Outbox>>,writes:Arc<Semaphore>,decision_tokens:Arc<Mutex<BTreeMap<String,DecisionToken>>>)->Result<Self> {
        let target=endpoint.node_id().to_owned();
        if let Endpoint::Local(grant)=&endpoint {ensure!(grant.network_id()==proof.network_id && grant.client_id()==key.device_id(),"local grant belongs to another cached profile");}
        let authority=proof.verify_offline(&key,&target)?;let network=proof.network_id.clone();
        let (events,_)=broadcast::channel(128);
        Ok(Self {key,connection:Connection::disconnected(),target,network,authority,endpoint,outbox,events,generation:uuid::Uuid::new_v4().to_string(),writes,decision_tokens,active:Arc::new(AtomicBool::new(false)),revoked:Arc::new(AtomicBool::new(false)),write_gate:Arc::new(AtomicU8::new(proof.write_gate))})
    }
    pub async fn reconnect_or_offline(&self,endpoint:Endpoint)->Result<Self> {
        match self.reconnect(endpoint.clone()).await {
            Ok(client)=>Ok(client),
            Err(error) if crate::connection::transport_failure(&error)=>{
                let target=endpoint.node_id().to_owned();let proof=self.stored(move|db|db.cached_profile(&target)).await?;
                let next=Self::offline_generation(self.key.clone(),endpoint,proof,self.outbox.clone(),self.writes.clone(),self.decision_tokens.clone())?;
                self.close();Ok(next)
            }
            Err(error)=>Err(error),
        }
    }
    pub fn is_online(&self)->bool {self.active.load(Ordering::Acquire) && self.connection.is_open()}
    pub fn target_node(&self)->&str {&self.target}
    pub fn offline_block_reason(&self)->Option<OfflineBlockReason> {match self.authority() {ClientAuthority::PrivateDataOnly {reason}=>Some(reason),_=>None}}
    pub async fn open_local(key:Arc<ClientKey>,grant:crate::LocalGrant,outbox_dir:PathBuf)->Result<Self> {
        Self::open(key,Endpoint::Local(grant),outbox_dir).await
    }
    async fn authenticate(key:&ClientKey, endpoint:&Endpoint)->Result<(Connection,String,ClientAuthority,crate::profile::CachedProfile)> {
        let network = match endpoint {Endpoint::Local(grant)=>{ensure!(grant.client_id()==key.device_id(),"local grant belongs to another client");grant.network_id()},_=>key.network_id().context("client is not enrolled")?}.to_owned();
        ensure!(key.network_id().is_none_or(|known|known==network),"entry belongs to another personal network");
        let connection = Connection::open_endpoint(&endpoint, &key.identity).await?;
        let authentication=connection.call("authenticate", json!({})).await?;
        if authentication["revoked"]==true || authentication["node_revoked"]==true {return Err(Revoked.into());}
        ensure!(authentication["network_id"]==network && authentication["principal"]["device_id"]==key.device_id(),"principal identity mismatch");
        let mut directory = connection.call("directory", json!({})).await?;
        let state: vibeface_secure::membership::NetworkState = serde_json::from_value(directory["state"].take())?;
        ensure!(state.genesis.network_id == network, "Node answered for another network");
        let now = directory["now_ms"].as_u64().context("directory time missing")?;
        let view = state.project_at(now)?;
        let authority=if key.network_id().is_some() && authentication["principal"]["source"]=="membership" {
            let member=view.client(key.device_id())?;
            ensure!(authentication["principal"]["can_manage"]==member.manage,"principal permission mismatch");
            ClientAuthority::Membership {can_manage:member.manage}
        } else {
            ensure!(matches!(endpoint,Endpoint::Local(_)) && matches!(authentication["principal"]["source"].as_str(),Some("local_session"|"membership")),"client has no enrolled membership or local grant");
            ClientAuthority::LocalSession
        };
        let node = view.member(endpoint.node_id())?;
        ensure!(node.kind == vibeface_secure::membership::IdentityKind::Node, "endpoint is not a network Node");
        let proof=crate::profile::CachedProfile {network_id:network.clone(),client_id:key.device_id().into(),target_node:endpoint.node_id().into(),state,observed_at_ms:now,write_gate:0,revoked:false};
        Ok((connection,network,authority,proof))
    }
    async fn attach(key:Arc<ClientKey>,endpoint:Endpoint,connection:Connection,network:String,authority:ClientAuthority,mut proof:crate::profile::CachedProfile,outbox:Arc<Mutex<Outbox>>,writes:Arc<Semaphore>,decision_tokens:Arc<Mutex<BTreeMap<String,DecisionToken>>>)->Result<Self> {
        let event_network=network.clone();
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
                    Err(broadcast::error::RecvError::Closed) => { let _=updates.send(ClientEvent {generation:token.clone(),network_id:event_network.clone(),payload:json!({"type":"disconnected"})}); break; }
                };
                let disconnected=payload["type"]=="disconnected";
                let _ = updates.send(ClientEvent { generation:token.clone(), network_id:event_network.clone(), payload });
                if disconnected {break;}
            }
        });
        connection.call("subscribe",json!({})).await?;
        let write_state=connection.call("write_state",json!({})).await?;
        let gate=write_gate_value(&write_state)?;proof.write_gate=gate;
        let cache=outbox.clone();tokio::task::spawn_blocking(move||cache.lock().map_err(|_|anyhow::anyhow!("outbox lock poisoned"))?.cache_profile(&proof)).await??;
        Ok(Self {key,connection,target:endpoint.node_id().into(),network,authority,endpoint,outbox,events,generation,writes,decision_tokens,active:Arc::new(AtomicBool::new(true)),revoked:Arc::new(AtomicBool::new(false)),write_gate:Arc::new(AtomicU8::new(gate))})
    }
    /// A new Noise generation shares the original durable outbox. Existing requests retain their original target.
    pub async fn reconnect(&self,endpoint:Endpoint)->Result<Self> {
        let (connection,network,authority,proof)=match Self::authenticate(&self.key,&endpoint).await {Ok(value)=>value,Err(error)=>{if error.is::<Revoked>() {let target=endpoint.node_id().to_owned();if target==self.target {self.revoked.store(true,Ordering::Release);self.connection.close();}self.stored(move|db|db.invalidate_profile(&target)).await?;}return Err(error);}};
        ensure!(network==self.network,"reconnect cannot switch outbox networks");
        let next=Self::attach(self.key.clone(),endpoint,connection,network,authority,proof,self.outbox.clone(),self.writes.clone(),self.decision_tokens.clone()).await?;
        self.close();
        Ok(next)
    }
    /// Stops queued calls. A request already crossing the transport remains unknown until reconciled.
    pub fn close(&self) { self.active.store(false,Ordering::Release);self.connection.close(); }
    fn require_active(&self)->Result<()> { ensure!(self.active.load(Ordering::Acquire) && !self.revoked.load(Ordering::Acquire),"client generation is closed or revoked");Ok(()) }
    pub fn take_decision_token(&self, operation_id:&str)->Result<Option<DecisionToken>> {
        Ok(self.decision_tokens.lock().map_err(|_|anyhow::anyhow!("token lock poisoned"))?.remove(operation_id))
    }
    pub fn generation(&self) -> &str { &self.generation }
    pub fn network_id(&self) -> &str { &self.network }
    pub fn authority(&self)->ClientAuthority {if self.revoked.load(Ordering::Acquire) {ClientAuthority::PrivateDataOnly {reason:OfflineBlockReason::Revoked}} else {self.authority}}
    pub fn can_write(&self)->bool {matches!(self.authority,ClientAuthority::Membership {..}) && !self.revoked.load(Ordering::Acquire)}
    pub fn can_save_locally(&self)->bool {self.can_write() && self.write_gate.load(Ordering::Acquire)!=2}
    pub fn can_control(&self)->bool {self.is_online() && self.can_write()}
    pub fn can_send_online(&self)->bool {self.active.load(Ordering::Acquire) && self.connection.is_open() && self.write_gate.load(Ordering::Acquire)==1 && self.can_write()}
    pub fn can_configure_node(&self)->bool {!self.revoked.load(Ordering::Acquire) && matches!(self.authority,ClientAuthority::LocalSession|ClientAuthority::Membership {can_manage:true})}
    pub fn management_session(&self)->Result<crate::Session<'_>> {
        self.require_active()?;ensure!(self.can_write(),"local session does not grant personal-network membership");
        Ok(self.key.session(self.connection.clone(),self.endpoint.clone()))
    }
    pub fn client_id(&self) -> &str { self.key.device_id() }
    pub fn subscribe(&self) -> broadcast::Receiver<ClientEvent> { self.events.subscribe() }
    async fn stored<T: Send + 'static>(&self, task: impl FnOnce(&mut Outbox)->Result<T> + Send + 'static) -> Result<T> {
        let outbox=self.outbox.clone();
        tokio::task::spawn_blocking(move || { let mut store=outbox.lock().map_err(|_|anyhow::anyhow!("outbox lock poisoned"))?; task(&mut store) }).await?
    }
    pub async fn next_input_sequence(&self, conversation_id: &str, target: &InputTarget) -> Result<u64> {
        ensure!(self.can_save_locally(),"this profile cannot stage network inputs");
        let lane=input_lane(conversation_id,target)?;
        self.stored(move|db|db.reserve_sequence(&lane)).await
    }
    pub async fn pending(&self) -> Result<Vec<OperationSnapshot>> { self.stored(|db|Ok(db.snapshots()?.into_iter().filter(|snapshot|snapshot.state!=OperationState::Accepted).collect())).await }
    pub async fn review_settings(&self,id:&str)->Result<crate::SettingsReview> {
        let id=id.to_owned();let target=self.target.clone();self.stored(move|db|{let review=db.review_settings(&id)?;ensure!(review.target_node==target,"return to the original Node before reviewing this operation");Ok(review)}).await
    }
    pub async fn close_unknown_settings(&self,review:crate::SettingsReview,note:String)->Result<Vec<OperationSnapshot>> {
        ensure!(review.network_id==self.network_id() && review.client_id==self.client_id() && review.target_node==self.target,"review belongs to another profile or Node");
        self.stored(move|db|db.close_unknown_settings(&review,&note)).await
    }
    pub async fn dismiss_rejected(&self,id:&str)->Result<()> {
        self.require_active()?;let id=id.to_owned();self.stored(move|db|db.dismiss_rejected(&id)).await
    }
    pub async fn store_attachment(&self, name: String, data: Vec<u8>) -> Result<Attachment> {
        ensure!(self.can_save_locally(),"this profile cannot retain new input attachments");
        let revoked=self.revoked.clone();self.stored(move|db|{ensure!(!revoked.load(Ordering::Acquire),"profile was revoked");db.store_attachment(&name,&data)}).await
    }
    pub async fn retain_attachments(&self,owner:&str,references:Vec<Attachment>)->Result<()> {
        ensure!(self.can_save_locally(),"this profile cannot retain input drafts");let owner=owner.to_owned();
        let revoked=self.revoked.clone();self.stored(move|db|{ensure!(!revoked.load(Ordering::Acquire),"profile was revoked");db.retain_attachments(&owner,&references)}).await
    }
    pub async fn release_attachments(&self,owner:&str)->Result<()> {
        self.require_active()?;let active=self.active.clone();let owner=owner.to_owned();
        self.stored(move|db|{ensure!(active.load(Ordering::Acquire),"draft belongs to a closed client");db.release_attachments(&owner)}).await
    }
    pub async fn attachment_owners(&self)->Result<Vec<String>> {self.stored(|db|db.attachment_owners()).await}
    /// The owning UI supplies all current selections. Pending writes protect their own content independently.
    pub async fn collect_attachments(&self,keep:Vec<Attachment>)->Result<()> {
        self.require_active()?;let active=self.active.clone();
        self.stored(move|db|{ensure!(active.load(Ordering::Acquire),"selection belongs to a closed client");db.collect_attachments(&keep)}).await
    }
    pub async fn read(&self, request: ReadRequest) -> Result<Value> {
        self.require_active()?;let update_gate=matches!(&request,ReadRequest::WriteState);
        if update_gate && self.write_gate.load(Ordering::Acquire)!=2 {self.write_gate.store(0,Ordering::Release);}
        let (rpc, method, mut params) = request.wire();
        let value=if rpc {params["method"]=json!(method);self.connection.call("rpc",params).await?} else {self.connection.call(method,params).await?};
        if update_gate {
            let mut gate=write_gate_value(&value)?;if self.write_gate.load(Ordering::Acquire)==2 {gate=2;}
            self.write_gate.store(gate,Ordering::Release);let target=self.target.clone();self.stored(move|db|db.cache_write_gate(&target,gate)).await?;
        }
        Ok(value)
    }
    pub async fn stage(&self,action:Action)->Result<OperationSnapshot> {self.stage_after(action,vec![]).await}
    pub async fn stage_after(&self,action:Action,mut prerequisites:Vec<String>)->Result<OperationSnapshot> {
        ensure!(if safety_action(&action) {self.can_write()} else {self.can_save_locally()},"this profile cannot stage this intent");
        if matches!(&action,Action::Delivery(_)|Action::NodeSettings(_)|Action::ProviderSettings(_)|Action::Triage(_)|Action::Relay {..}|Action::CreateDecisionToken {..}|Action::RevokeDecisionToken {..}|Action::ConfirmWritable|Action::TestWebhook) {ensure!(matches!(self.authority,ClientAuthority::Membership {can_manage:true}),"operation requires a managing member");}
        prerequisites.sort();prerequisites.dedup();
        let client=self.clone();
        self.stored(move|db|client.stage_in(db,action,prerequisites)).await
    }
    fn stage_in(&self,db:&mut Outbox,mut action:Action,prerequisites:Vec<String>)->Result<OperationSnapshot> {
        let local_control=matches!(&action,Action::NodeSettings(_)|Action::ProviderSettings(_)|Action::Triage(_)|Action::Relay {..}|Action::CreateDecisionToken {..}|Action::RevokeDecisionToken {..}|Action::ConfirmWritable|Action::TestWebhook);
        ensure!(!self.revoked.load(Ordering::Acquire) && (self.write_gate.load(Ordering::Acquire)!=2 || safety_action(&action) || local_control),"known revoked or forked profile cannot stage new network work");
        let known=match &action {
            Action::Input {input,..}=>Some(input.message_id.clone()),
            Action::Workspace {command}=>Some(self.operation_id("workspace",&json!({"type":"workspace_commanded","command":command}))?),
            Action::Memory {conversation_id,memory_id,base,after}=>Some(self.operation_id(conversation_id,&json!({"type":"memory_controlled","memory_id":memory_id,"base":base,"after":after}))?),
            _=>None,
        };
        if let Some(id)=known && let Some(original)=db.find(&id)? {
            ensure!(original.snapshot.target_node==self.target && original.prerequisites==prerequisites,"original operation target or prerequisite cannot change");
            if let Action::Input {conversation_id,input}=&action {
                let request:Value=serde_json::from_slice(&original.bytes)?;
                let mut stored=request["params"].clone();stored.as_object_mut().context("stored input params missing")?.remove("method");stored.as_object_mut().context("stored input params missing")?.remove("attestation");
                let mut wanted=serde_json::to_value(input)?;wanted["conversation_id"]=json!(conversation_id);
                if input.route.sequence==0 {wanted["sequence"]=stored["sequence"].clone();}
                ensure!(wanted==stored,"original input contents or CAS cannot change");
            }
            return Ok(original.snapshot);
        }
        if let Action::Input {conversation_id,input}=&mut action && input.route.sequence==0 {input.route.sequence=db.reserve_sequence(&input_lane(conversation_id,&input.route.target)?)?;}
        let mut operation=self.prepare(action)?;operation.prerequisites=prerequisites;
        db.prepare(&operation)?;Ok(operation.snapshot)
    }
    /// Local staging survives transport loss; only an admitted writer changes Prepared to Unknown.
    pub async fn submit(&self,action:Action)->Result<OperationSnapshot> {
        let node_operator=matches!(&action,Action::NodeSettings(_)|Action::ProviderSettings(_)|Action::Triage(_)|Action::Relay {..}|Action::TestWebhook|Action::CreateDecisionToken {..}|Action::RevokeDecisionToken {..}|Action::ConfirmWritable);
        let allowed=if node_operator {self.can_configure_node() && self.active.load(Ordering::Acquire)} else if matches!(&action,Action::Delivery(_)) {self.can_save_locally() && matches!(self.authority,ClientAuthority::Membership {can_manage:true})} else if safety_action(&action) {self.can_write()} else {self.can_save_locally()};
        ensure!(allowed,"this client is not authorized for this operation");
        let client=self.clone();let staged=self.stored(move|db|client.stage_in(db,action,vec![])).await?;
        if staged.state!=OperationState::Prepared {return Ok(staged);}
        self.send_staged(&staged.id).await
    }
    pub async fn send_staged(&self,id:&str)->Result<OperationSnapshot> {
        let permit=self.writes.clone().acquire_owned().await?;let client=self.clone();let id=id.to_owned();
        tokio::spawn(async move{let _permit=permit;client.send_prepared(id).await}).await?
    }
    async fn send_prepared(&self,id:String)->Result<OperationSnapshot> {
        let lookup=id.clone();let operation=self.stored(move|db|db.get(&lookup)).await?;
        ensure!(operation.snapshot.target_node==self.target,"original operation cannot switch target");
        if operation.snapshot.state!=OperationState::Prepared || !self.active.load(Ordering::Acquire) || !self.connection.is_open() {return Ok(operation.snapshot);}
        let request:Value=serde_json::from_slice(&operation.bytes)?;
        let safety=safety_request(&request);
        let network_write=(request["method"]=="rpc" || request["method"]=="delivery") && !safety;
        if safety && !self.can_control() {return Ok(operation.snapshot);}
        if network_write && !self.can_send_online() {return Ok(operation.snapshot);}
        if request["method"]=="delivery" {ensure!(matches!(self.authority,ClientAuthority::Membership {can_manage:true}),"delivery requires current management permission");}
        if !network_write && !safety {ensure!(self.can_configure_node(),"Node operation requires current owner permission");}
        let check=id.clone();if !self.stored(move|db|db.can_begin(&check)).await? {return self.stored(move|db|Ok(db.get(&id)?.snapshot)).await;}
        let sent = async {
            if let Some(conversation_id)=&operation.upload_scope {for attachment in &operation.attachments {self.upload(conversation_id,attachment).await?;}}
            self.require_active()?;
            let before=self.clone();let begin_id=id.clone();
            let callback:crate::connection::BeforeWrite=Box::new(move||Box::pin(async move {
                before.require_active()?;
                let active=before.active.clone();let gate=before.write_gate.clone();let revoked=before.revoked.clone();
                before.stored(move|db|{ensure!(active.load(Ordering::Acquire) && !revoked.load(Ordering::Acquire) && (!network_write || gate.load(Ordering::Acquire)==1),"operation was not admitted under current authority");db.begin(&begin_id).map(|_|())}).await
            }));
            self.connection.call_staged(operation.bytes.clone(),callback).await
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
                } else { self.stored(move|db|db.observe_unknown(&id,result)).await }
            }
            // Generic RPC errors do not prove no side effect; only method-specific receipts settle a write.
            Err(_) => self.stored(move|db|Ok(db.get(&id)?.snapshot)).await,
        }
    }
    pub async fn reconcile(&self, id: &str) -> Result<OperationSnapshot> {
        self.require_active()?;
        let id=id.to_owned();
        let op=self.stored(move|db|db.get(&id)).await?;
        ensure!(op.snapshot.target_node==self.target && op.snapshot.network_id==self.network_id() && op.snapshot.client_id==self.client_id(),"operation belongs to another endpoint or identity");
        if op.snapshot.state==OperationState::Prepared {return self.send_staged(&op.snapshot.id).await;}
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
            Reconciliation::Settings {kind}=>self.connection.call(crate::settings::read_method(*kind),json!({})).await?,
            Reconciliation::StopReply {conversation_id,..}=>self.connection.call("rpc",json!({"method":"conversation","conversation_id":conversation_id})).await?,
            Reconciliation::StopWork {conversation_id,work_id}=>self.connection.call("rpc",json!({"method":"work_detail","conversation_id":conversation_id,"work_id":work_id})).await?,
            Reconciliation::Manual => return Ok(op.snapshot),
        };
        let state=match &op.reconciliation {
            Reconciliation::Input {message_id,target,..} => {
                ensure!(result["message_id"]==*message_id && result["target"]==serde_json::to_value(target)?,"receipt identifies another input");
                match result["stage"].as_str() {
                    Some("entry_accepted"|"owner_accepted"|"submitting"|"provider_accepted"|"associated"|"refused") => {
                        let mut original:Value=serde_json::from_slice(&op.bytes)?;
                        let mut body=original["params"].take();let object=body.as_object_mut().context("stored input body missing")?;
                        let scope=object.remove("conversation_id").context("input conversation missing")?;
                        let attestation=object.remove("attestation").context("input signature missing")?;
                        object.remove("method");object.insert("type".into(),json!("input_posted"));
                        let proof=self.connection.call("rpc",json!({"method":"signed_receipt","scope_id":scope,"body":body,"attestation":attestation})).await?;
                        ensure!(proof["network_id"]==op.snapshot.network_id && proof["client_id"]==op.snapshot.client_id && proof["scope_id"]==scope,"input signed receipt identity mismatch");
                        match proof["receipt"].as_str() {Some("recorded")=>{ensure!(valid_hash(&proof["event_hash"]),"input has no signed event evidence");Some(OperationState::Accepted)},Some("not_seen_here")=>None,_=>anyhow::bail!("invalid signed input receipt")}
                    },
                    Some("unknown") => None,
                    _ => anyhow::bail!("invalid input receipt stage"),
                }
            }
            Reconciliation::Memory {memory_id,..} => {
                if !result["current"].is_null() {validate_memory_state(&result["current"],memory_id)?;}
                match result["receipt"].as_str() {
                    Some("recorded") => { ensure!(result["current"]["memory_id"]==*memory_id && result["current"]["metadata"]["revision"]==op.snapshot.id,"memory receipt identity mismatch"); Some(OperationState::Accepted) },
                    Some("superseded") => {ensure!(result["current"]["metadata"]["revision"]!=op.snapshot.id,"superseded memory receipt mismatch");Some(OperationState::Rejected)},
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
            Reconciliation::Settings {kind}=>{
                let request:Value=serde_json::from_slice(&op.bytes)?;
                crate::settings::goal_matches(*kind,&request["params"],&result).then_some(OperationState::Accepted)
            }
            Reconciliation::StopReply {conversation_id,trigger}=>{
                ensure!(result["conversation"]["conversation_id"]==*conversation_id,"stop readback belongs to another conversation");
                result["scheduler"]["jobs"].as_array().and_then(|jobs|jobs.iter().find(|job|job["trigger"]==*trigger)).filter(|job|terminal_reply(&job["state"])).map(|_|OperationState::Accepted)
            }
            Reconciliation::StopWork {conversation_id,work_id}=>{
                ensure!(result["work"]["work_id"]==*work_id && result["work"]["conversation_id"]==*conversation_id,"stop readback belongs to another work");
                matches!(result["work"]["outcome"].as_str(),Some("completed"|"failed"|"interrupted"|"rejected")).then_some(OperationState::Accepted)
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
            self.require_active()?;
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
            Action::Input {conversation_id,..}=>Some(conversation_id.clone()),Action::Workspace {..}=>Some("workspace".into()),_=>None,
        };
        let mut id=uuid::Uuid::new_v4().to_string();
        let mut sequence=None;
        let mut revision=None;
        let mut attachments=vec![];
        let mut reconciliation=Reconciliation::Manual;
        let (rpc,method,mut params,lane,signature)=match action {
            Action::Input {conversation_id,input} => {
                vibeface_protocol::validate_id(&input.message_id).map_err(anyhow::Error::msg)?;
                vibeface_protocol::validate_id(&conversation_id).map_err(anyhow::Error::msg)?;
                ensure!(input.text.len()<=65536 && (!input.text.trim().is_empty() || !input.attachments.is_empty()),"input needs text or an attachment within 65536 bytes");
                ensure!(input.route.sequence>0,"input sequence must be positive");
                id=input.message_id.clone(); sequence=Some(input.route.sequence); revision=input.route.expected_revision;
                attachments=input.attachments.clone();
                reconciliation=Reconciliation::Input {conversation_id:conversation_id.clone(),message_id:id.clone(),target:input.route.target.clone()};
                let mut body=serde_json::to_value(&input)?; body["type"]=json!("input_posted");
                let mut params=serde_json::to_value(&input)?; params["conversation_id"]=json!(conversation_id);
                let lane=input_lane(&conversation_id,&input.route.target)?;
                (true,"message",params,lane,Some((conversation_id,body)))
            }
            Action::Workspace {command} => {
                attachments=match &command {WorkspaceCommand::PostChannelMessage {attachments,..}|WorkspaceCommand::ControlInput {action:InputControl::Edit {attachments,..},..}=>attachments.clone(),_=>vec![]};
                reconciliation=Reconciliation::Workspace;
                id=self.operation_id("workspace",&json!({"type":"workspace_commanded","command":command}))?;
                revision=serde_json::to_value(&command)?["expected_revision"].as_u64();
                let body=json!({"type":"workspace_commanded","command_id":id,"command":command});
                (true,"workspace_command",json!({"command_id":id,"command":command}),"workspace".into(),Some(("workspace".into(),body)))
            }
            Action::Memory {conversation_id,memory_id,base,after} => {
                ensure!(base.epoch==1,"unsupported memory epoch");
                if let Some(version)=&base.version {ensure!(valid_hash(&json!(version.revision))&&valid_hash(&json!(version.source_event_hash)),"memory version must identify exact source hashes");}
                if let Some(text)=&after {ensure!(!text.trim().is_empty() && text.chars().count()<=500,"a memory must contain 1-500 characters");}
                let body=json!({"type":"memory_controlled","memory_id":memory_id,"base":base,"after":after});
                id=self.operation_id(&conversation_id,&body)?;
                reconciliation=Reconciliation::Memory {conversation_id:conversation_id.clone(),memory_id:memory_id.clone()};
                (true,"control_memory",json!({"conversation_id":conversation_id,"memory_id":memory_id,"base":base,"after":after,"operation_id":id}),format!("memory:{conversation_id}:{memory_id}"),Some((conversation_id,body)))
            }
            Action::Schedule {conversation_id,schedule_id,action} => {
                let body=json!({"type":"schedule_controlled","schedule_id":schedule_id,"action":action});
                (true,"control_schedule",json!({"conversation_id":conversation_id,"schedule_id":schedule_id,"action":action}),format!("schedule:{conversation_id}:{schedule_id}"),Some((conversation_id,body)))
            }
            Action::StopReply {conversation_id,trigger} => {reconciliation=Reconciliation::StopReply {conversation_id:conversation_id.clone(),trigger:trigger.clone()};(true,"cancel_manager",json!({"conversation_id":conversation_id,"trigger":trigger}),format!("reply:{conversation_id}:{trigger}"),None)},
            Action::StopWork {conversation_id,work_id} => {reconciliation=Reconciliation::StopWork {conversation_id:conversation_id.clone(),work_id:work_id.clone()};(true,"interrupt_work",json!({"conversation_id":conversation_id,"work_id":work_id}),format!("work:{conversation_id}:{work_id}"),None)},
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
            Action::ConfirmWritable => (false,"confirm_writable",json!({}),"node-write-status".into(),None),
            Action::AckNotification {conversation_id,notification_id} => {
                let body=json!({"type":"notification_acked","notification_id":notification_id,"client_id":self.client_id()});
                (true,"ack",json!({"notification_id":notification_id}),format!("notification:{notification_id}"),Some((conversation_id,body)))
            }
            Action::ResolveCheck {conversation_id,check_id,work_id,outcome,note} => {
                let note=note.trim().to_owned();
                let signature=if check_id.starts_with("work:") {
                    let work_id=work_id.context("work ID required for work resolution")?;
                    ensure!(check_id==format!("work:{work_id}"),"check does not identify this work");
                    let outcome=outcome.as_ref().context("work outcome required")?;
                    Some((conversation_id.clone(),json!({"type":"work_finished","work_id":work_id,"outcome":outcome,"result_text":note})))
                } else {None};
                (true,"resolve",json!({"conversation_id":conversation_id,"check_id":check_id,"outcome":outcome,"note":note}),format!("check:{conversation_id}:{check_id}"),signature)
            }
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
        let lane=if !rpc && method!="delivery" {format!("node:{}:{lane}",self.target)} else {lane};
        let method=if rpc { params["method"]=json!(method); "rpc" } else {method};
        let request=json!({"id":id,"method":method,"params":params});
        if let Some(kind)=crate::settings::kind(&request) {reconciliation=Reconciliation::Settings {kind};}
        let bytes=serde_json::to_vec(&request)?;
        Ok(Operation {snapshot:OperationSnapshot {id,network_id:self.network_id().into(),client_id:self.client_id().into(),target_node:self.target.clone(),state:OperationState::Prepared,sequence,result:None},lane,expected_revision:revision,bytes,reconciliation,attachments,upload_scope,prerequisites:vec![]})
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
        "cancel_manager"=>{
            if result["unconfirmed"]==true {return Ok(None);}
            ensure!(result["job"]["trigger"]==request["params"]["trigger"] && result["job"]["conversation_id"]==request["params"]["conversation_id"],"stop receipt identifies another reply");
            Ok((result["cancel_requested"]==true || terminal_reply(&result["job"]["state"]["state"])).then_some(OperationState::Accepted))
        }
        "interrupt_work"=>{
            ensure!(result["work_id"]==request["params"]["work_id"],"stop receipt identifies another work");
            Ok((result["interrupt_requested"]==true || result["confirmed"]==true || result["admission"]["state"]=="owner_accepted").then_some(OperationState::Accepted))
        }
        "message" => {
            ensure!(result["message_id"]==op.snapshot.id,"input response has another message ID");
            if result["accepted"]==true { Ok(Some(OperationState::Accepted)) }
            else if result["stored"]==false && result["code"]=="input_not_saved" { Ok(Some(OperationState::Rejected)) }
            else { Ok(None) }
        }
        "workspace_command" => {
            ensure!(result["command_id"]==op.snapshot.id,"workspace response has another command ID");
            if result["stored"]==false && result["code"]=="workspace_command_not_saved" { Ok(Some(OperationState::Rejected)) }
            else if valid_hash(&result["event_hash"]) { Ok(Some(OperationState::Accepted)) }
            else { Ok(None) }
        }
        "control_memory" => {
            ensure!(result["operation_id"]==op.snapshot.id,"memory response has another operation ID");
            if !result["current"].is_null() {validate_memory_state(&result["current"],request["params"]["memory_id"].as_str().context("memory ID missing")?)?;}
            match result["state"].as_str() {
                Some("applied"|"already_recorded")=>{ensure!(result["current"]["metadata"]["revision"]==op.snapshot.id,"memory receipt does not prove original revision");Ok(Some(OperationState::Accepted))},
                Some("superseded")=>{ensure!(result["current"]["metadata"]["revision"]!=op.snapshot.id,"superseded receipt still names original revision");Ok(Some(OperationState::Rejected))},
                Some("conflict")=>Ok(Some(OperationState::Rejected)),
                _=>Ok(None),
            }
        }
        _=> { ensure!(result.is_object(),"mutation result must be an object"); Ok(Some(OperationState::Accepted)) }
    }
}

impl ReadRequest {
    fn wire(self)->(bool,&'static str,Value) {
        match self {
            Self::ConversationHistory(request)=>(true,"conversation_history",serde_json::to_value(request).expect("history request contains only wire fields")),
            Self::WriteState=>(false,"write_state",json!({})),
            Self::Home=>(true,"bots",json!({})),
            Self::Conversation {conversation_id}=>(true,"conversation",json!({"conversation_id":conversation_id})),
            Self::Workspace=>(true,"workspace_state",json!({})),
            Self::WorkspaceOverview=>(true,"workspace_overview",json!({})),
            Self::WorkspaceHistory {channel_id,task_id,limit}=>{
                let mut params=json!({"limit":limit});
                if let Some(id)=channel_id{params["channel_id"]=json!(id);}
                if let Some(id)=task_id{params["task_id"]=json!(id);}
                (true,"workspace_history",params)
            },
            Self::Nodes=>(true,"nodes",json!({})),
            Self::NodeSettings=>(false,"node_settings",json!({})),
            Self::Logs=>(false,"logs",json!({})),
            Self::LogsFiltered(filter)=>(false,"logs",serde_json::to_value(filter).expect("log filter contains only serializable fields")),
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
            Self::ArtifactBytes {conversation_id,blob_hash,offset}=>(true,"read_artifact",json!({"conversation_id":conversation_id,"blob_hash":blob_hash,"offset":offset,"binary":true})),
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

    #[tokio::test]
    async fn lost_receipt_and_restart_query_the_original_operation_without_duplicate_effect() -> Result<()> {
        let dir=std::env::temp_dir().join(format!("native-restart-{}",uuid::Uuid::new_v4()));
        let mut key=ClientKey::create(&dir.join("key"),"controlled fixture password")?;
        let node=Identity::generate()?;
        let node_secret=node.secret_hex();
        let genesis=Genesis::create(&Identity::generate()?,Member::client(&key.identity,"fixture",true)?,Member::node(&node,"fixture-node")?)?;
        let network=genesis.network_id.clone();let state=NetworkState::new(genesis)?;
        let fixture_client_id=key.device_id().to_owned();
        let effects=Arc::new(AtomicUsize::new(0));
        let input=InputRequest {message_id:"original-message".into(),route:InputRoute {target:InputTarget::BotMain {bot_id:"bot".into()},intent:InputIntent::Continue,sequence:1,expected_revision:None},text:"controlled input".into(),attachments:vec![],report_results:Some(true)};
        for cycle in 0..2 {
            let listener=tokio::net::TcpListener::bind("127.0.0.1:0").await?;
            let origin=format!("http://{}",listener.local_addr()?);
            let entry=crate::Entry {origin,node_id:node.device_id()};
            key.remember(&network,entry.clone())?;
            let fixture_node=Identity::from_secret_hex(&node_secret)?;let fixture_state=state.clone();let counter=effects.clone();let fixture_input=input.clone();let fixture_client_id=fixture_client_id.clone();
            let task=tokio::spawn(async move {
                let (mut socket,mut channel)=server(listener,&fixture_node).await?;
                loop {
                    let request=receive(&mut socket,&mut channel).await?;
                    let result=match request["method"].as_str() {
                        Some("authenticate")=>json!({"network_id":fixture_state.genesis.network_id,"principal":{"source":"membership","device_id":fixture_client_id,"can_manage":true}}),
                        Some("subscribe")=>json!({}),
                        Some("write_state")=>json!({"state":"writable"}),
                        Some("directory")=>json!({"state":fixture_state,"now_ms":vibeface_protocol::now_ms()}),
                        Some("rpc")=>match request["params"]["method"].as_str() {
                            Some("message")=> {counter.fetch_add(1,Ordering::SeqCst); socket.close(None).await?;break;},
                            Some("input_receipt")=>json!({"message_id":fixture_input.message_id,"target":fixture_input.route.target,"stage":"entry_accepted","retry_safe":false,"control_event":null,"actual_turn_id":null,"reason":null}),
                            Some("signed_receipt")=>json!({"network_id":fixture_state.genesis.network_id,"client_id":fixture_client_id,"scope_id":"chat","receipt":"recorded","event_hash":"a".repeat(64)}),
                            _=>anyhow::bail!("unexpected fixture RPC"),
                        },
                        _=>anyhow::bail!("unexpected fixture method"),
                    };
                    send(&mut socket,&mut channel,json!({"id":request["id"],"result":result})).await?;
                    if request["params"]["method"]=="signed_receipt" {break;}
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
        let operation=Operation {snapshot:OperationSnapshot {id:"message-1".into(),network_id:"n".into(),client_id:"c".into(),target_node:"t".into(),state:OperationState::Unknown,sequence:Some(1),result:None},lane:"chat".into(),expected_revision:None,bytes:serde_json::to_vec(&json!({"params":{"method":"message"}}))?,reconciliation:Reconciliation::Manual,attachments:vec![],upload_scope:None,prerequisites:vec![]};
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

fn input_lane(conversation_id:&str,target:&InputTarget)->Result<String> {
    let target=match target {InputTarget::Task {task_id}|InputTarget::ChannelTask {task_id,..}=>json!(["workspace_task",task_id]),other=>serde_json::to_value(other)?};
    Ok(format!("input:{conversation_id}:{target}"))
}

fn validate_memory_state(state:&Value,memory_id:&str)->Result<()> {
    ensure!(state["memory_id"]==memory_id && (state["text"].is_string()||state["text"].is_null()),"memory state identity or text is invalid");
    let metadata=&state["metadata"];
    ensure!(metadata["epoch"]==1 && valid_hash(&metadata["revision"]) && valid_hash(&metadata["source_event_hash"]) && metadata["control_revision"].as_u64().is_some_and(|v|v<=u32::MAX as u64),"invalid memory metadata");
    ensure!(matches!(metadata["origin"].as_str(),Some("agent"|"user")) && metadata["user_controlled"].is_boolean() && metadata["essential"].is_boolean(),"invalid memory protection metadata");Ok(())
}

fn write_gate_value(value:&Value)->Result<u8> {
    Ok(match value["state"].as_str() {Some("writable")=>1,Some("forked")=>2,Some("read_only"|"aligning")=>3,_=>anyhow::bail!("invalid Node write state")})
}

#[cfg(test)]
mod offline_tests {
    use super::*;
    use crate::connection::tests::{receive,send,server};
    use vibeface_secure::{Identity,membership::{Genesis,Member,NetworkState}};

    struct Evidence {counts:BTreeMap<String,u32>,inputs:BTreeMap<String,Value>,write_state:String,controls:Vec<Value>,settings:Value}
    async fn fixture(node_secret:&str,state:NetworkState,client_id:String,evidence:Arc<Mutex<Evidence>>,drop_message:Option<String>)->Result<(Endpoint,tokio::task::JoinHandle<Result<()>>)> {
        let node=Identity::from_secret_hex(node_secret)?;
        let listener=tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let entry=crate::Entry {origin:format!("http://{}",listener.local_addr()?),node_id:node.device_id()};
        let task=tokio::spawn(async move {
            let (mut socket,mut channel)=server(listener,&node).await?;
            loop {
                let request=match receive(&mut socket,&mut channel).await {Ok(value)=>value,Err(_)=>break};
                let result=match request["method"].as_str() {
                    Some("authenticate")=>json!({"network_id":state.genesis.network_id,"principal":{"source":"membership","device_id":client_id,"can_manage":true}}),
                    Some("directory")=>json!({"state":state,"now_ms":vibeface_protocol::now_ms()}),
                    Some("subscribe")=>json!({}),
                    Some("write_state")=>json!({"state":evidence.lock().expect("fixture evidence").write_state}),
                    Some("configure_node")=>{
                        let settings={let mut saved=evidence.lock().expect("fixture evidence");*saved.counts.entry("configure_node".into()).or_default()+=1;saved.settings=request["params"].clone();saved.settings.clone()};
                        if drop_message.as_deref()==Some("@configure_node") {socket.close(None).await?;break;}
                        json!({"settings":settings})
                    }
                    Some("configure_provider")=>{
                        {let mut saved=evidence.lock().expect("fixture evidence");*saved.counts.entry("configure_provider".into()).or_default()+=1;}
                        if drop_message.as_deref()==Some("@configure_provider") {socket.close(None).await?;break;}
                        json!({"settings":evidence.lock().expect("fixture evidence").settings})
                    }
                    Some("node_settings")=>json!({"settings":evidence.lock().expect("fixture evidence").settings,"providers":[{"provider":"codex","env":{"SYNTHETIC_KEY":{"set":true}}}]}),
                    Some("rpc")=>match request["params"]["method"].as_str() {
                        Some("cancel_manager")=>{
                            let params=&request["params"];evidence.lock().expect("fixture evidence").controls.push(params.clone());
                            json!({"cancel_requested":true,"job":{"conversation_id":params["conversation_id"],"trigger":params["trigger"],"state":{"state":"cancelled"}}})
                        }
                        Some("interrupt_work")=>{
                            let params=&request["params"];evidence.lock().expect("fixture evidence").controls.push(params.clone());
                            json!({"work_id":params["work_id"],"interrupt_requested":true,"confirmed":true})
                        }
                        Some("message")=>{
                            let params=&request["params"];
                            let id=params["message_id"].as_str().context("message ID")?.to_owned();
                            let mut body=params.clone();
                            let object=body.as_object_mut().context("input body")?;
                            let attestation=object.remove("attestation").context("signature")?;
                            object.remove("method");object.remove("conversation_id");object.insert("type".into(),json!("input_posted"));
                            vibeface_secure::verify(&client_id,vibeface_secure::join::ATTESTATION_DOMAIN,&vibeface_secure::join::attestation_payload(&state.genesis.network_id,"chat",&body.to_string())?,attestation["signature"].as_str().context("signature bytes")?)?;
                            {
                                let mut saved=evidence.lock().expect("fixture evidence");
                                *saved.counts.entry(id.clone()).or_default()+=1;
                                saved.inputs.insert(id.clone(),params.clone());
                            }
                            if drop_message.as_deref()==Some(id.as_str()) {socket.close(None).await?;break;}
                            json!({"accepted":true,"message_id":id})
                        }
                        Some("signed_receipt")=>{
                            let query=&request["params"];let id=query["body"]["message_id"].as_str().context("signed receipt ID")?;
                            let saved=evidence.lock().expect("fixture evidence");let original=saved.inputs.get(id).context("unrecorded signed input")?;
                            let mut body=original.clone();let object=body.as_object_mut().context("recorded input")?;
                            object.remove("method");object.remove("conversation_id");object.remove("attestation");object.insert("type".into(),json!("input_posted"));
                            let recorded=body==query["body"] && original["attestation"]==query["attestation"];
                            json!({"network_id":state.genesis.network_id,"client_id":client_id,"scope_id":"chat","receipt":if recorded {"recorded"} else {"not_seen_here"},"event_hash":"a".repeat(64)})
                        }
                        Some("input_receipt")=>{
                            let id=request["params"]["message_id"].as_str().context("receipt ID")?;
                            let saved=evidence.lock().expect("fixture evidence");
                            let input=saved.inputs.get(id).context("fixture has not accepted input")?;
                            json!({"message_id":id,"target":input["target"],"stage":"entry_accepted","retry_safe":false,"control_event":null,"actual_turn_id":null,"reason":null})
                        }
                        _=>anyhow::bail!("unexpected fixture RPC"),
                    },
                    _=>anyhow::bail!("unexpected fixture method"),
                };
                send(&mut socket,&mut channel,json!({"id":request["id"],"result":result})).await?;
            }
            Ok(())
        });
        Ok((Endpoint::Gateway(entry),task))
    }
    fn input(id:&str,target:InputTarget)->Action {
        Action::Input {conversation_id:"chat".into(),input:InputRequest {message_id:id.into(),route:InputRoute {target,intent:InputIntent::Continue,sequence:0,expected_revision:Some(1)},text:format!("input {id}"),attachments:vec![],report_results:Some(true)}}
    }
    fn provider_change()->ProviderSettings {ProviderSettings {provider:Provider::Codex,bot:None,execution:None,execution_model:None,bin:None,app_server:None,connect_local:false,env:BTreeMap::from([("SYNTHETIC_KEY".into(),Some("synthetic-secret-never-in-review".into()))])}}
    #[tokio::test]
    async fn lost_settings_ack_uses_goal_readback_and_private_changes_require_explicit_group_review()->Result<()> {
        let dir=std::env::temp_dir().join(format!("native-settings-review-{}",uuid::Uuid::new_v4()));
        let mut key=ClientKey::create(&dir.join("key"),"fixture password")?;let node=Identity::generate()?;
        let state=NetworkState::new(Genesis::create(&Identity::generate()?,Member::client(&key.identity,"client",true)?,Member::node(&node,"node")?)?)?;
        let evidence=Arc::new(Mutex::new(Evidence {counts:BTreeMap::new(),inputs:BTreeMap::new(),write_state:"writable".into(),controls:vec![],settings:json!({"roots":[],"manager":null})}));
        let (endpoint,first)=fixture(&node.secret_hex(),state.clone(),key.device_id().into(),evidence.clone(),Some("@configure_node".into())).await?;
        let Endpoint::Gateway(entry)=&endpoint else {unreachable!()};key.remember(&state.genesis.network_id,entry.clone())?;let key=Arc::new(key);
        let client=Client::open(key.clone(),endpoint,dir.join("outbox")).await?;
        let settings=NodeSettings {roots:vec![PathBuf::from("/controlled/project")],manager:None};
        let request=client.submit(Action::NodeSettings(settings.clone())).await?;
        assert_eq!(request.state,OperationState::Unknown);drop(client);first.await??;
        let (endpoint,second)=fixture(&node.secret_hex(),state.clone(),key.device_id().into(),evidence.clone(),None).await?;
        let client=Client::open(key.clone(),endpoint,dir.join("outbox")).await?;
        evidence.lock().expect("fixture").settings=json!({"roots":["/different"],"manager":null});
        assert_eq!(client.reconcile(&request.id).await?.state,OperationState::Unknown,"mismatched public state is not proof");
        evidence.lock().expect("fixture").settings=serde_json::to_value(settings)?;
        assert_eq!(client.reconcile(&request.id).await?.state,OperationState::Accepted);
        assert_eq!(evidence.lock().expect("fixture").counts.get("configure_node"),Some(&1));
        client.close();drop(client);second.await??;
        let (endpoint,third)=fixture(&node.secret_hex(),state.clone(),key.device_id().into(),evidence.clone(),Some("@configure_provider".into())).await?;
        let client=Client::open(key.clone(),endpoint,dir.join("outbox")).await?;
        let parent=client.submit(Action::ProviderSettings(provider_change())).await?;
        assert_eq!(parent.state,OperationState::Unknown);drop(client);third.await??;
        let (endpoint,fourth)=fixture(&node.secret_hex(),state,key.device_id().into(),evidence.clone(),None).await?;
        let client=Client::open(key,endpoint,dir.join("outbox")).await?;
        assert_eq!(client.reconcile(&parent.id).await?.state,OperationState::Unknown,"a secret-set flag cannot prove which value was applied");
        let task=client.stage_after(Action::Workspace {command:WorkspaceCommand::CreateTask {task_id:"later-task".into(),channel_id:None,parent_task_id:None,owner_bot:"bot".into(),title:"held".into(),request:"must not run".into(),expected_result:"none".into(),source_events:vec![],dependencies:vec![],reply_only:false}},vec![parent.id.clone()]).await?;
        let child=client.stage_after(input("dependent-input",InputTarget::Task {task_id:"later-task".into()}),vec![task.id.clone()]).await?;
        let same_lane=client.stage(Action::ProviderSettings(provider_change())).await?;
        let review=client.review_settings(&parent.id).await?;
        assert!(review.has_secret_changes);assert!(!format!("{review:?}").contains("synthetic-secret-never-in-review"));assert_eq!(review.dependent_ids.len(),3);
        let mut foreign=review.clone();foreign.target_node="other-node".into();
        assert!(client.close_unknown_settings(foreign,"wrong target".into()).await.is_err());
        let closed=client.close_unknown_settings(review,"Reviewed the original Node; do not resend this operation or its queued dependents.".into()).await?;
        assert_eq!(closed.len(),4);assert!(closed.iter().all(|snapshot|snapshot.state==OperationState::ClosedByUser));
        let parent_id=parent.id.clone();assert_eq!(client.stored(move|db|db.settle(&parent_id,OperationState::Accepted,json!({"late":true}))).await?.state,OperationState::ClosedByUser);
        for id in [&task.id,&child.id,&same_lane.id] {assert_eq!(client.send_staged(id).await?.state,OperationState::ClosedByUser);}
        assert_eq!(evidence.lock().expect("fixture").counts,BTreeMap::from([("configure_node".into(),1),("configure_provider".into(),1)]));
        client.close();drop(client);fourth.await??;std::fs::remove_dir_all(dir)?;Ok(())
    }
    #[tokio::test]
    async fn current_member_safety_stops_work_on_read_only_and_forked_nodes_without_starting_work()->Result<()> {
        let dir=std::env::temp_dir().join(format!("native-stop-policy-{}",uuid::Uuid::new_v4()));
        let mut key=ClientKey::create(&dir.join("key"),"fixture password")?;let node=Identity::generate()?;
        let state=NetworkState::new(Genesis::create(&Identity::generate()?,Member::client(&key.identity,"client",true)?,Member::node(&node,"node")?)?)?;
        let evidence=Arc::new(Mutex::new(Evidence {counts:BTreeMap::new(),inputs:BTreeMap::new(),write_state:"read_only".into(),controls:vec![],settings:json!({"roots":[],"manager":null})}));
        let (endpoint,server)=fixture(&node.secret_hex(),state.clone(),key.device_id().into(),evidence.clone(),None).await?;
        let Endpoint::Gateway(entry)=&endpoint else {unreachable!()};key.remember(&state.genesis.network_id,entry.clone())?;
        let client=Client::open(Arc::new(key),endpoint,dir.join("outbox")).await?;
        assert!(client.can_control());assert!(!client.can_send_online());
        let staged=client.submit(input("must-stay-local",InputTarget::BotMain {bot_id:"bot".into()})).await?;
        assert_eq!(staged.state,OperationState::Prepared);
        assert_eq!(client.submit(Action::StopReply {conversation_id:"chat".into(),trigger:"message:original".into()}).await?.state,OperationState::Accepted);
        evidence.lock().expect("fixture").write_state="forked".into();client.read(ReadRequest::WriteState).await?;
        assert!(client.can_control());assert!(!client.can_save_locally());
        assert!(client.stage(input("must-not-stage",InputTarget::BotMain {bot_id:"bot".into()})).await.is_err());
        assert_eq!(client.submit(Action::StopWork {conversation_id:"chat".into(),work_id:"original-work".into()}).await?.state,OperationState::Accepted);
        assert_eq!(client.send_staged("must-stay-local").await?.state,OperationState::Prepared);
        let saved=evidence.lock().expect("fixture");assert!(saved.counts.is_empty());
        assert_eq!(saved.controls,vec![json!({"method":"cancel_manager","conversation_id":"chat","trigger":"message:original"}),json!({"method":"interrupt_work","conversation_id":"chat","work_id":"original-work"})]);drop(saved);
        client.close();drop(client);server.await??;std::fs::remove_dir_all(dir)?;Ok(())
    }
    #[tokio::test]
    async fn offline_queue_restart_and_concurrent_flush_preserve_exact_inputs_without_duplicate_effects()->Result<()> {
        let dir=std::env::temp_dir().join(format!("native-offline-{}",uuid::Uuid::new_v4()));
        let mut key=ClientKey::create(&dir.join("key"),"client fixture password")?;
        let node=Identity::generate()?;
        let state=NetworkState::new(Genesis::create(&Identity::generate()?,Member::client(&key.identity,"client",true)?,Member::node(&node,"node")?)?)?;
        let evidence=Arc::new(Mutex::new(Evidence {counts:BTreeMap::new(),inputs:BTreeMap::new(),write_state:"writable".into(),controls:vec![],settings:json!({"roots":[],"manager":null})}));
        let (entry,first)=fixture(&node.secret_hex(),state.clone(),key.device_id().into(),evidence.clone(),None).await?;
        let Endpoint::Gateway(gateway)=&entry else {unreachable!()};key.remember(&state.genesis.network_id,gateway.clone())?;
        let original_endpoint=entry.clone();
        let client=Client::open(Arc::new(key),entry,dir.join("outbox")).await?;
        let task=InputTarget::Task {task_id:"task".into()};
        let channel_task=InputTarget::ChannelTask {channel_id:"channel".into(),task_id:"task".into(),bot_id:"bot".into()};
        let a=client.stage(input("offline-a",task.clone())).await?;
        assert_eq!((a.state,a.sequence),(OperationState::Prepared,Some(1)));
        let original=client.stored(|db|Ok(db.get("offline-a")?.bytes)).await?;
        client.close();first.await??;
        assert!(client.can_save_locally());drop(client);
        // No server listens here. The new process verifies its signed cache and only touches private storage.
        let key=Arc::new(ClientKey::open(&dir.join("key"),"client fixture password")?);
        let client=Client::open_or_offline(key,original_endpoint,dir.join("outbox")).await?;
        assert!(!client.is_online());assert!(client.can_save_locally());
        assert_eq!(client.pending().await?.iter().map(|pending|(pending.id.as_str(),pending.state)).collect::<Vec<_>>(),vec![("offline-a",OperationState::Prepared)]);
        assert_eq!(client.stored(|db|Ok(db.get("offline-a")?.bytes)).await?,original);
        let b=client.stage(input("offline-b",channel_task)).await?;
        let c=client.stage(input("offline-c",task.clone())).await?;
        assert_eq!((b.sequence,c.sequence),(Some(2),Some(3)),"task and channel-task share one durable sequence");
        assert_eq!(client.send_staged("offline-a").await?.state,OperationState::Prepared,"a closed transport never admits the write");
        assert!(evidence.lock().expect("fixture").counts.is_empty());
        let (entry,second)=fixture(&node.secret_hex(),state.clone(),client.client_id().into(),evidence.clone(),Some("offline-a".into())).await?;
        let next=client.reconnect(entry).await?;drop(client);
        let (one,two)=tokio::join!(next.send_staged("offline-a"),next.send_staged("offline-a"));one?;two?;second.await??;
        assert_eq!(evidence.lock().expect("fixture").counts.get("offline-a"),Some(&1));
        let d=next.stage_after(input("offline-d",InputTarget::BotMain {bot_id:"other".into()}),vec!["offline-a".into()]).await?;
        assert_eq!(d.state,OperationState::Prepared);
        let e=next.stage(input("offline-e",task)).await?;
        assert_eq!((e.state,e.sequence),(OperationState::Prepared,Some(4)),"new input can be durably saved behind an unknown outcome");
        drop(next);
        let key=Arc::new(ClientKey::open(&dir.join("key"),"client fixture password")?);
        let (entry,third)=fixture(&node.secret_hex(),state,key.device_id().into(),evidence.clone(),None).await?;
        let client=Client::open(key,entry,dir.join("outbox")).await?;
        assert_eq!(client.stored(|db|Ok(db.get("offline-a")?.bytes)).await?,original);
        assert_eq!(client.send_staged("offline-b").await?.state,OperationState::Prepared,"unknown input blocks its lane");
        assert_eq!(client.send_staged("offline-d").await?.state,OperationState::Prepared,"unknown prerequisite blocks another lane");
        assert_eq!(evidence.lock().expect("fixture").counts.len(),1);
        assert_eq!(client.reconcile("offline-a").await?.state,OperationState::Accepted);
        assert_eq!(client.send_staged("offline-c").await?.state,OperationState::Prepared,"earlier saved inputs preserve order");
        let (one,two)=tokio::join!(client.send_staged("offline-b"),client.send_staged("offline-b"));one?;two?;
        assert_eq!(client.send_staged("offline-e").await?.state,OperationState::Prepared,"saved inputs cannot overtake an earlier sequence");
        assert_eq!(client.send_staged("offline-c").await?.state,OperationState::Accepted);
        assert_eq!(client.send_staged("offline-e").await?.state,OperationState::Accepted);
        assert_eq!(client.send_staged("offline-d").await?.state,OperationState::Accepted);
        assert!(client.pending().await?.is_empty());
        assert_eq!(evidence.lock().expect("fixture").counts,BTreeMap::from([("offline-a".into(),1),("offline-b".into(),1),("offline-c".into(),1),("offline-d".into(),1),("offline-e".into(),1)]));
        client.close();drop(client);third.await??;
        std::fs::remove_dir_all(dir)?;Ok(())
    }
}

fn safety_action(action:&Action)->bool {matches!(action,Action::StopReply {..}|Action::StopWork {..})}
fn safety_request(request:&Value)->bool {request["method"]=="rpc" && matches!(request["params"]["method"].as_str(),Some("cancel_manager"|"interrupt_work"))}
fn terminal_reply(state:&Value)->bool {matches!(state.as_str(),Some("cancelled"|"interrupted"|"completed"|"failed"|"rejected"))}
