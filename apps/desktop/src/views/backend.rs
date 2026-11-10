//! Single background owner of client state. No IO or wire parsing belongs in GPUI render.
mod cache;
mod commands;
mod delivery;
mod forms;
mod history;
mod inputs;
mod operator;
mod profiles;
mod records;
mod session;
mod settings;
use crate::{
    host::{BackgroundAction, DesktopHost, HostSnapshot, NodeIdentity, NodeMode},
    ui::*,
    views::forms::Editor,
};
use anyhow::{Context, Result, ensure};
use base64::Engine;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{collections::BTreeMap, path::PathBuf, sync::Arc, time::Duration};
use tokio::sync::{broadcast, mpsc, watch};
use vibeface_client::{
    Action, Client, ClientKey, DeliveryRead, Endpoint, Entry, MemoryBase, MemoryVersion, Provider,
    ProviderSettings, ReadRequest,
};
use vibeface_protocol::workspace::{
    InputIntent, InputRequest, InputRoute, InputTarget, ScheduleAction, WorkspaceCommand,
};

#[derive(Clone)]
pub enum HostAction {
    Refresh,
    Controller,
    Select(NodeIdentity),
    Start,
    Stop,
    Background(BackgroundAction),
    Remember(PathBuf),
    StartCandidate(crate::host::LocalNode),
    ClearSelection,
    Preferences(crate::host::Preferences),
}
#[derive(Clone)]
pub enum IdentityAction {
    Directory,
    Invite,
    Requests,
    Grant { device_id: String, manage: bool },
    Revoke(String),
    Rename { device_id: String, name: String },
    Recovery(bool),
    ConfirmJoin(String),
    GrantJoin { join_id: String, manage: bool },
    RejectJoin(String),
}
#[derive(Clone)]
pub enum ActiveRecord {
    None,
    Work {
        conversation_id: String,
        work_id: String,
    },
    Channel(String),
    Task(String),
}
#[derive(Clone)]
pub enum SettingsKind {
    Node,
    Triage,
    Relay,
    Provider(Provider),
}
#[derive(Clone)]
pub enum Command {
    RestoreProfile {
        conversation_id: String,
        record: crate::views::history::OverwrittenRecord,
    },
    LoadHistory {
        conversation_id: String,
        section: vibeface_protocol::history::HistorySection,
    },
    SendStaged(String),
    ReviewSettings(String),
    AcknowledgeOperatorUnknown(String),
    SwitchProfile(String),
    WatchRecord(ActiveRecord),
    ConfirmIdentityBackup(String),
    ReconcileIdentity,
    DiscardIdentity,
    Scoped {
        generation: u64,
        command: Box<Command>,
    },
    Refresh,
    Disconnect,
    Conversation(String),
    Save {
        editor: Editor,
        values: BTreeMap<String, String>,
        conversation_id: Option<String>,
    },
    Input {
        conversation_id: String,
        draft: ComposerDraft,
    },
    Workspace(WorkspaceCommand),
    StopReply(String),
    StopWork {
        conversation_id: String,
        work_id: String,
    },
    Answer {
        conversation_id: String,
        question_id: String,
        text: String,
    },
    RetryReply {
        conversation_id: String,
        message_id: String,
    },
    Reconcile(String),
    ResumeDraft(String),
    DeleteBot(String),
    PrimaryBot(String),
    DeleteMemory {
        conversation_id: String,
        memory: Memory,
    },
    Schedule {
        conversation_id: String,
        schedule_id: String,
        action: String,
    },
    Identity(IdentityAction),
    Host(HostAction),
    DeliveryConfigure {
        enabled: bool,
        charity: bool,
        paid: bool,
    },
    DeliveryConfirm(TransferOrder),
    DeliveryAccept(String),
    DeliveryReconcile(String),
    DeliverySave {
        order: TransferOrder,
        path: PathBuf,
    },
    ReadMemory {
        conversation_id: String,
        memory_id: String,
    },
    ScopeContext {
        conversation_id: String,
        target: InputTarget,
    },
    ArtifactPreview {
        conversation_id: String,
        file: vibeface_protocol::workspace::Attachment,
        offset: u64,
    },
    ArtifactSave {
        conversation_id: String,
        file: vibeface_protocol::workspace::Attachment,
        path: PathBuf,
    },
    DiagnosticsSave(PathBuf),
    EditSettings(SettingsKind),
    EditOwnerSettings(SettingsKind),
    ProviderRead {
        provider: Provider,
        probe: bool,
    },
    TestWebhook,
    DecisionTokenRevoke(String),
    ReadTriage,
    Ack(Notification),
    ResolveCheck(CoordinationCheck),
    ConfirmWritable,
    History {
        channel_id: Option<String>,
        task_id: Option<String>,
    },
    WorkDetail {
        conversation_id: String,
        work_id: String,
    },
    ResolveWork {
        conversation_id: String,
        work_id: String,
        outcome: vibeface_client::ResolveOutcome,
    },
    NodeSettings(String),
    ReadLogs {
        search: String,
    },
    Transfers,
    SelectAttachmentScope(String),
    ReleaseAttachment {
        conversation_id: String,
        hash: String,
    },
    Attachments {
        conversation_id: String,
        paths: Vec<PathBuf>,
    },
}
pub enum Event {
    Scoped {
        generation: u64,
        profile_id: Option<String>,
        event: Box<Event>,
    },
    Snapshot(Arc<Snapshot>),
    Host(HostSnapshot),
    Notice(String),
    Error(String),
    Detail(Vec<(String, String)>),
    Attachments {
        conversation_id: String,
        attachments: Vec<vibeface_protocol::workspace::Attachment>,
    },
    InputStored(String),
    FormSaved(String),
    EditForm {
        editor: Editor,
        values: BTreeMap<&'static str, String>,
    },
    CopySecret(zeroize::Zeroizing<String>),
    ShutdownComplete,
}
#[derive(Clone)]
enum ReconnectTarget {
    Network(Endpoint),
    Local(crate::host::NodeIdentity),
}
struct ProfileClient {
    client: Client,
    key_network: Option<String>,
}
struct Reconnected {
    attempt: u64,
    profile_id: String,
    result: Result<Client>,
}
struct Completed {
    generation: String,
    input: Option<String>,
    form_id: Option<String>,
    copy_token: bool,
    delivery: bool,
    result: Result<vibeface_client::OperationSnapshot>,
}
#[derive(Clone, Deserialize, Serialize)]
struct DurableInputDraft {
    network_id: String,
    client_id: String,
    target_node: String,
    conversation_id: String,
    input: InputRequest,
    prerequisite: Option<WorkspaceCommand>,
    #[serde(default)]
    prerequisite_id: Option<String>,
    fingerprint: String,
}
impl DurableInputDraft {
    fn validate(&self) -> Result<()> {
        let hash = |value: &str| {
            value.len() == 64
                && value
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        };
        ensure!(
            hash(&self.network_id)
                && hash(&self.client_id)
                && hash(&self.target_node)
                && hash(&self.fingerprint),
            "原生待发输入的身份或指纹无效"
        );
        vibeface_protocol::validate_id(&self.conversation_id).map_err(anyhow::Error::msg)?;
        vibeface_protocol::validate_id(&self.input.message_id).map_err(anyhow::Error::msg)?;
        crate::views::records::validate_attachments(&self.input.attachments)?;
        Ok(())
    }
}
pub struct Backend {
    identity_busy: bool,
    recovery: mpsc::Receiver<Reconnected>,
    recovered: mpsc::Sender<Reconnected>,
    reconnect_task: Option<tokio::task::JoinHandle<()>>,
    completion: mpsc::Sender<Completed>,
    pushes: Option<broadcast::Receiver<vibeface_client::ClientEvent>>,
    host: DesktopHost,
    client_profiles: Option<crate::host::profiles::ProfileRegistry>,
    credential_base: String,
    retired_clients: BTreeMap<String, ProfileClient>,
    active_record: ActiveRecord,
    last_record_read: tokio::time::Instant,
    form_id: Option<String>,
    form_owner: Option<String>,
    reconnect: Option<ReconnectTarget>,
    retry_at: tokio::time::Instant,
    retry_attempt: u32,
    identity_plan: Option<(ClientKey, vibeface_client::IdentityPlan)>,
    active_entry: Option<Entry>,
    key: Option<Arc<ClientKey>>,
    client: Option<Client>,
    state: Snapshot,
    history_state: Option<Box<crate::views::history::HistoryState>>,
    conversation: Option<String>,
    out: mpsc::Sender<Event>,
    snapshots: watch::Sender<Arc<Snapshot>>,
    attachments: BTreeMap<String, Vec<vibeface_protocol::workspace::Attachment>>,
}
impl Backend {
    pub fn spawn(
        host: DesktopHost,
        runtime: &tokio::runtime::Handle,
        credential_base: String,
    ) -> (
        mpsc::Sender<Command>,
        mpsc::Receiver<Event>,
        watch::Receiver<Arc<Snapshot>>,
        watch::Sender<bool>,
        tokio::task::JoinHandle<()>,
    ) {
        let (tx, mut rx) = mpsc::channel(64);
        let (out, events) = mpsc::channel(64);
        let (snapshots, snapshot_rx) = watch::channel(Arc::new(Snapshot::default()));
        let (stop, mut stopping) = watch::channel(false);
        let (completion, mut completed) = mpsc::channel(64);
        let (recovered, recovery) = mpsc::channel(8);
        #[cfg(test)]
        Self::test_phase("spawn-prepare");
        let actor = async move {
            #[cfg(test)]
            Self::test_phase("actor-first-poll");
            let registry = crate::host::profiles::ProfileRegistry::open(&host.client_storage_dir());
            let registry_error = registry.as_ref().err().map(ToString::to_string);
            let mut backend = Self {
                identity_busy: false,
                recovery,
                recovered,
                reconnect_task: None,
                completion,
                pushes: None,
                host,
                client_profiles: registry.ok(),
                credential_base,
                retired_clients: BTreeMap::new(),
                active_record: ActiveRecord::None,
                last_record_read: tokio::time::Instant::now(),
                form_id: None,
                form_owner: None,
                reconnect: None,
                retry_at: tokio::time::Instant::now(),
                retry_attempt: 0,
                identity_plan: None,
                active_entry: None,
                key: None,
                client: None,
                state: Snapshot::default(),
                history_state: None,
                conversation: None,
                out,
                snapshots,
                attachments: BTreeMap::new(),
            };
            backend.publish_host().await;
            #[cfg(test)]
            Self::test_phase("actor-host-published");
            if let Some(error) = registry_error {
                let _ = backend.emit(Event::Error(error)).await;
            } else {
                let _ = backend.publish_profiles();
                let _ = backend.load_operator_pending().await;
            }
            #[cfg(test)]
            Self::test_phase("actor-ready");
            let mut timer = tokio::time::interval(Duration::from_secs(2));
            'commands: loop {
                tokio::select! {biased;_=stopping.changed()=>break,
                recovered=backend.recovery.recv()=>{if let Some(recovered)=recovered{if recovered.attempt==backend.state.generation&&backend.current_profile_id().ok().as_deref()==Some(recovered.profile_id.as_str()){backend.reconnect_task=None;match recovered.result{Ok(client)=>{backend.pushes=Some(client.subscribe());backend.client=Some(client);backend.retry_attempt=0;if let Err(error)=backend.refresh().await{backend.state.online=false;backend.state.writable=false;backend.state.can_control=false;backend.state.status=format!("重新连接后读取尚未完成：{error}");backend.publish();}else{let _=backend.flush_prepared().await;}},Err(error)=>{backend.state.online=false;backend.state.writable=false;backend.state.can_control=false;backend.state.can_save_locally=backend.client.as_ref().is_some_and(|client|client.can_save_locally())&&(backend.state.synced||backend.state.cached||backend.state.projection_valid);backend.state.status=format!("当前尚未重新验证连接：{error}");backend.publish();}}}else if let Ok(client)=recovered.result{client.close();}}},
                result=completed.recv()=>{if let Some(completed)=result{if backend.client.as_ref().is_some_and(|client|client.generation()==completed.generation){match completed.result{Ok(operation)=>{if operation.state==vibeface_client::OperationState::Accepted{if let Err(error)=backend.finish_accepted_draft(&operation.id).await{let _=backend.emit(Event::Error(format!("原输入已接收，本机提醒尚未清理：{error}"))).await;}let _=backend.flush_prepared().await;}if let Some(id)=completed.form_id{let _=backend.emit(Event::FormSaved(id)).await;}if completed.copy_token{if let Some(client)=backend.client.as_ref(){if let Ok(Some(token))=client.take_decision_token(&operation.id){let _=backend.emit(Event::CopySecret(zeroize::Zeroizing::new(token.expose().to_owned()))).await;}}}if let Some(text)=completed.input{let _=backend.emit(Event::InputStored(text)).await;}let _=backend.emit(Event::Notice(format!("操作 {}：{}。",operation.id,operation_label(operation.state)))).await;if let Err(error)=backend.refresh().await{let _=backend.emit(Event::Error(format!("原操作已保存，工作区重新读取失败：{error}"))).await;}if completed.delivery||!backend.state.delivery.orders.is_empty()||backend.state.delivery.enabled{if let Err(error)=backend.refresh_delivery().await{let _=backend.emit(Event::Error(format!("原订单已保存，条款或进度尚未重新确认：{error}"))).await;}}},Err(error)=>{let _=backend.emit(Event::Error(format!("操作未完成：{error}"))).await;}}}}},
                push=async{match backend.pushes.as_mut(){Some(pushes)=>pushes.recv().await,None=>std::future::pending().await}}=>{match push{Ok(event)if backend.client.as_ref().is_some_and(|client|client.generation()==event.generation&&client.network_id()==event.network_id)=>{if let Err(error)=backend.push(event.payload).await{backend.state.status=format!("实时记录需要重新读取：{error}");let _=backend.refresh().await;}},Err(broadcast::error::RecvError::Lagged(_))=>{backend.state.status="实时记录有缺口，正在重新读取。".into();let _=backend.refresh().await;},Err(broadcast::error::RecvError::Closed)=>{backend.pushes=None;backend.state.online=false;backend.state.writable=false;backend.state.can_control=false;backend.state.status="连接已断开，未确认的操作不会重发。".into();backend.publish();},_=>{}}},
                command=rx.recv()=>{let Some(command)=command else{break};let command=match command{Command::Scoped{generation,command}=>{if generation!=backend.state.generation{let _=backend.emit(Event::Error("连接已经切换，旧页面操作未提交。".into())).await;continue}*command},command=>command};#[cfg(test)] Self::test_phase("dispatch-command");let result=tokio::select!{biased;_=stopping.changed()=>break 'commands,result=backend.run(command)=>result};if let Err(error)=result{let _=backend.emit(Event::Error(format!("操作未完成：{error}"))).await;}},
                _=timer.tick()=>{let result=tokio::select!{biased;_=stopping.changed()=>break 'commands,result=backend.maintain()=>result};if let Err(error)=result{backend.state.online=false;backend.state.writable=false;backend.state.can_control=false;backend.state.can_save_locally=backend.client.as_ref().is_some_and(|client|client.can_save_locally())&&(backend.state.synced||backend.state.cached||backend.state.projection_valid);backend.state.status="连接中断，显示最近记录。结果未知的操作须先核对。".into();backend.state.error=Some(error.to_string());backend.publish();}}}
            }
            backend.cancel_reconnect().await;
            if let Some(client) = &backend.client {
                client.close();
            }
            match backend.host.shutdown().await {
                Ok(()) => {
                    let _ = backend.emit(Event::ShutdownComplete).await;
                }
                Err(error) => {
                    let _ = backend
                        .emit(Event::Error(format!("Node 未能正常停止：{error}")))
                        .await;
                }
            }
        };
        #[cfg(test)]
        {
            use std::io::Write as _;
            let _ = writeln!(
                std::io::stderr(),
                "native actor outer future: {} bytes",
                std::mem::size_of_val(&actor)
            );
        }
        let task = runtime.spawn(actor);
        #[cfg(test)]
        Self::test_phase("spawn-returned");
        (tx, events, snapshot_rx, stop, task)
    }

    async fn emit(&self, event: Event) -> std::result::Result<(), mpsc::error::SendError<Event>> {
        self.out
            .send(Event::Scoped {
                generation: self.state.generation,
                profile_id: self.current_profile_id().ok(),
                event: Box::new(event),
            })
            .await
    }
    fn publish(&self) {
        let mut state = self.state.clone();
        state.identity_busy = self.identity_busy || self.reconnect_task.is_some();
        if let Some(registry) = &self.client_profiles {
            state.profiles = registry.profiles().to_vec();
            state.profile_id = Some(registry.selected_id().as_str().into());
            state.credential_scope = registry
                .credential_scope_from_base(registry.selected_id(), &self.credential_base)
                .ok();
        }
        self.snapshots.send_replace(Arc::new(state));
    }
    async fn publish_host(&mut self) {
        self.state.can_configure_owner = self.host.snapshot().selected.is_some();
        self.state.can_configure = self
            .client
            .as_ref()
            .map(|client| client.can_configure_node())
            .unwrap_or(self.state.can_configure_owner);
        self.publish();
        let _ = self.emit(Event::Host(self.host.snapshot())).await;
    }
    fn client(&self) -> Result<&Client> {
        self.client.as_ref().context("请先连接个人网络")
    }
    async fn submit(&mut self, action: Action) -> Result<()> {
        let local_operator = matches!(
            &action,
            Action::NodeSettings(_)
                | Action::ProviderSettings(_)
                | Action::Triage(_)
                | Action::Relay { .. }
                | Action::TestWebhook
                | Action::CreateDecisionToken { .. }
                | Action::RevokeDecisionToken { .. }
        );
        if local_operator
            && (self.form_owner.is_some()
                || self.client.as_ref().is_none_or(|_| !self.state.online))
        {
            ensure!(self.state.can_configure_owner, "本机Node尚未验证为可管理");
            return self.operator_submit(action).await;
        }
        ensure!(
            self.state.writable
                || self.state.can_control
                    && matches!(&action, Action::StopReply { .. } | Action::StopWork { .. })
                || self.state.can_configure
                    && matches!(
                        &action,
                        Action::NodeSettings(_)
                            | Action::ProviderSettings(_)
                            | Action::Triage(_)
                            | Action::Relay { .. }
                            | Action::TestWebhook
                            | Action::CreateDecisionToken { .. }
                            | Action::RevokeDecisionToken { .. }
                            | Action::ConfirmWritable
                    ),
            "当前会话不能写入，请先加入并完成状态核对"
        );
        let copy_token = matches!(&action, Action::CreateDecisionToken { .. });
        let delivery = matches!(&action, Action::Delivery(_));
        let input = if let Action::Input { input, .. } = &action {
            Some(input.text.clone())
        } else {
            None
        };
        let client = self.client()?.clone();
        let generation = client.generation().to_owned();
        let completion = self.completion.clone();
        let form_id = self.form_id.clone();
        tokio::spawn(async move {
            let result = client.submit(action).await;
            let _ = completion
                .send(Completed {
                    generation,
                    input,
                    form_id,
                    copy_token,
                    delivery,
                    result,
                })
                .await;
        });
        Ok(())
    }
    async fn push(&mut self, event: Value) -> Result<()> {
        let payload = if event["type"] == "changed" {
            &event["payload"]
        } else {
            &event
        };
        match payload["type"].as_str() {
            Some("disconnected") => {
                self.state.online = false;
                self.state.writable = false;
                self.state.can_control = false;
                self.state.status = "连接已断开，正在重连；原操作不会重新签名。".into();
                self.publish();
            }
            Some("records") => {
                if Some(payload["conversation_id"].as_str().unwrap_or(""))
                    == self.conversation.as_deref()
                {
                    self.merge_history_push(payload.clone()).await?;
                }
            }
            Some("manager_progress" | "native_progress") => {
                if Some(payload["conversation_id"].as_str().unwrap_or(""))
                    != self.conversation.as_deref()
                {
                    return Ok(());
                }
                let gap = apply_progress(&mut self.state.live, payload)?;
                self.publish();
                if gap {
                    self.refresh().await?;
                }
            }
            _ => {
                self.refresh().await?;
                self.refresh_active_record().await?;
            }
        }
        Ok(())
    }

    async fn refresh(&mut self) -> Result<()> {
        #[cfg(test)]
        Self::test_phase("refresh-enter");
        let client = self.client()?.clone();
        let home = client.read(ReadRequest::Home).await?;
        let nodes = client.read(ReadRequest::Nodes).await?;
        let workspace = client.read(ReadRequest::Workspace).await?;
        let write_state = client.read(ReadRequest::WriteState).await?;
        let mut next = self.state.clone();
        next.local_session = matches!(
            client.authority(),
            vibeface_client::ClientAuthority::LocalSession
        );
        next.write_state = write_state["state"]
            .as_str()
            .context("写入状态缺失")?
            .into();
        next.writable = client.can_write() && next.write_state == "writable";
        next.can_control = client.can_control();
        next.can_save_locally = client.can_save_locally();
        next.projection_valid = true;
        next.can_manage = matches!(
            client.authority(),
            vibeface_client::ClientAuthority::Membership { can_manage: true }
        );
        next.can_configure = client.can_configure_node();
        next.can_configure_owner = self.host.snapshot().selected.is_some();
        next.bots = decode_list(&home, "bots")?;
        next.nodes = decode_list(&nodes, "nodes")?;
        next.tasks = decode_list(&workspace, "tasks")?;
        next.assignments = decode_list(&workspace, "assignments")?;
        next.channels = decode_list(&workspace, "channels")?;
        next.inputs = decode_list(&workspace, "inputs")?;
        next.questions = decode_list(&client.read(ReadRequest::Questions).await?, "questions")?;
        next.notifications = decode_list(
            &client.read(ReadRequest::Notifications).await?,
            "notifications",
        )?;
        if let Some(id) = self.conversation.clone() {
            let bot = next
                .bots
                .iter()
                .find(|bot| bot.conversation_id == id)
                .map(|bot| bot.bot_id.clone());
            let (conversation, reset) = self.read_conversation(&id, bot.as_deref()).await?;
            next.conversation = Arc::new(conversation);
            if reset {
                self.reset_history_authority().await?;
                return Ok(());
            }
        }
        next.drafts = load_draft_summaries(self.draft_dir()?).await?;
        next.pending = client
            .pending()
            .await?
            .into_iter()
            .map(|p| Pending {
                id: p.id,
                state: operation_label(p.state).into(),
                target_node: p.target_node,
            })
            .collect();
        next.online = true;
        next.private_block = None;
        next.cached = false;
        next.cached_at_ms = None;
        next.cache_omitted = 0;
        next.synced = next.write_state != "aligning";
        next.status = if next.local_session {
            "本机会话只读。加入个人网络后才能发送签名输入。".into()
        } else if next.writable {
            "已读取当前连接。未确认的操作保留在待处理列表。".into()
        } else {
            format!(
                "当前写入状态 {}，先核对分叉或同步状态后恢复。",
                next.write_state
            )
        };
        next.error = None;
        prune_terminal_progress(&mut next.live, &next.conversation);
        let became_writable = !self.state.writable && next.writable;
        self.state = next;
        self.publish();
        if let Err(error) = self.save_projection().await {
            let _ = self
                .emit(Event::Notice(format!(
                    "当前记录已读取，离线缓存未更新：{error}"
                )))
                .await;
        }
        if became_writable {
            self.flush_prepared().await?;
        }
        Ok(())
    }
}
fn memory_base(metadata: &MemoryMetadata) -> Result<MemoryBase> {
    Ok(MemoryBase {
        epoch: metadata.epoch.try_into()?,
        version: Some(MemoryVersion {
            revision: metadata.revision.clone(),
            control_revision: metadata.control_revision.try_into()?,
            source_event_hash: metadata.source_event_hash.clone(),
        }),
    })
}
fn settings_rows(value: &Value) -> Vec<(String, String)> {
    let mut rows = vec![];
    if let Some(providers) = value["providers"].as_array() {
        for provider in providers {
            rows.push((
                provider["provider"].as_str().unwrap_or("Provider").into(),
                format!(
                    "{} · {}",
                    provider["state"].as_str().unwrap_or("状态未知"),
                    provider["readiness"]["state"]
                        .as_str()
                        .unwrap_or("尚未探测")
                ),
            ));
        }
    }
    rows
}

fn same_terms(a: &TransferOrder, b: &TransferOrder) -> bool {
    a.order_id == b.order_id
        && a.intent_id == b.intent_id
        && a.mode == b.mode
        && a.acceptance == b.acceptance
        && a.source_peer == b.source_peer
        && a.receiver_peer == b.receiver_peer
        && a.provider_peer == b.provider_peer
        && a.name == b.name
        && a.bytes == b.bytes
        && a.sha256 == b.sha256
        && a.payment.chain_id == b.payment.chain_id
        && a.payment.asset == b.payment.asset
        && a.payment.delivery_fee == b.payment.delivery_fee
        && a.payment.reservation_fee == b.payment.reservation_fee
        && a.payment.priority_fee == b.payment.priority_fee
        && a.payment.total == b.payment.total
        && a.payment.gas_cap == b.payment.gas_cap
        && a.payment.payer == b.payment.payer
        && a.payment.provider_wallet == b.payment.provider_wallet
        && a.deadlines == b.deadlines
}

fn parse_optional_bool(value: &str) -> Result<Option<bool>> {
    match value {
        "" => Ok(None),
        "true" => Ok(Some(true)),
        "false" => Ok(Some(false)),
        _ => anyhow::bail!("请输入 true 或 false"),
    }
}

impl Command {
    pub fn requires_session(&self) -> bool {
        match self {
            Self::SwitchProfile(_)
            | Self::ConfirmIdentityBackup(_)
            | Self::ReconcileIdentity
            | Self::DiscardIdentity
            | Self::Host(_)
            | Self::Disconnect
            | Self::Refresh => false,
            Self::Save { editor, .. }
                if matches!(
                    editor,
                    Editor::Connect
                        | Editor::ConnectLocal
                        | Editor::CreateNetwork
                        | Editor::Join
                        | Editor::Recovery
                ) =>
            {
                false
            }
            _ => true,
        }
    }
}

async fn read_draft(path: PathBuf) -> Result<DurableInputDraft> {
    tokio::task::spawn_blocking(move || {
        let draft: DurableInputDraft =
            serde_json::from_slice(&vibeface_bridge::platform::privatefs::read_private_bounded(
                &path,
                1024 * 1024,
                "原生待发输入",
            )?)?;
        draft.validate()?;
        ensure!(
            path.file_stem().and_then(|stem| stem.to_str()) == Some(draft.fingerprint.as_str()),
            "原生待发输入文件与指纹不一致"
        );
        Ok(draft)
    })
    .await?
}
async fn write_draft(path: PathBuf, draft: DurableInputDraft) -> Result<()> {
    tokio::task::spawn_blocking(move || {
        draft.validate()?;
        vibeface_bridge::platform::privatefs::create_dir(
            path.parent().context("待发输入缺少父目录")?,
        )?;
        vibeface_bridge::platform::privatefs::write_atomic(&path, &serde_json::to_vec(&draft)?)
    })
    .await?
}
async fn load_draft_summaries(directory: PathBuf) -> Result<Vec<NativeDraftSummary>> {
    tokio::task::spawn_blocking(move || {
        if !directory.try_exists()? {
            return Ok(vec![]);
        }
        let mut drafts = vec![];
        for entry in std::fs::read_dir(directory)? {
            let path = entry?.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            let saved: DurableInputDraft = serde_json::from_slice(
                &vibeface_bridge::platform::privatefs::read_private_bounded(
                    &path,
                    1024 * 1024,
                    "原生待发输入",
                )?,
            )?;
            saved.validate()?;
            ensure!(
                path.file_stem().and_then(|stem| stem.to_str()) == Some(saved.fingerprint.as_str()),
                "原生待发输入文件与指纹不一致"
            );
            drafts.push(NativeDraftSummary {
                fingerprint: saved.fingerprint,
                text: saved.input.text,
                conversation_id: saved.conversation_id,
            });
        }
        Ok(drafts)
    })
    .await?
}

fn parse_optional_number<T: std::str::FromStr>(value: &str) -> Result<Option<T>>
where
    T::Err: std::error::Error + Send + Sync + 'static,
{
    if value.is_empty() {
        Ok(None)
    } else {
        Ok(Some(value.parse()?))
    }
}

async fn load_draft_attachments(
    directory: PathBuf,
) -> Result<Vec<vibeface_protocol::workspace::Attachment>> {
    tokio::task::spawn_blocking(move || {
        if !directory.try_exists()? {
            return Ok(vec![]);
        }
        let mut files = vec![];
        for entry in std::fs::read_dir(directory)? {
            let path = entry?.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            let saved: DurableInputDraft = serde_json::from_slice(
                &vibeface_bridge::platform::privatefs::read_private_bounded(
                    &path,
                    1024 * 1024,
                    "原生待发输入",
                )?,
            )?;
            saved.validate()?;
            files.extend(saved.input.attachments);
        }
        Ok(files)
    })
    .await?
}

async fn fetch_attachment(
    client: &Client,
    conversation: &str,
    file: &vibeface_protocol::workspace::Attachment,
) -> Result<Vec<u8>> {
    ensure!(file.size <= 64 * 1024 * 1024, "附件超过64MiB");
    let mut bytes = Vec::with_capacity(file.size as usize);
    let mut offset = 0;
    loop {
        let page = client
            .read(ReadRequest::ArtifactBytes {
                conversation_id: conversation.into(),
                blob_hash: file.hash.clone(),
                offset,
            })
            .await?;
        ensure!(
            page["blob_hash"] == file.hash
                && page["size"].as_u64() == Some(file.size)
                && page["offset"].as_u64() == Some(offset),
            "附件分块与原引用不一致"
        );
        let part = base64::engine::general_purpose::STANDARD
            .decode(page["data"].as_str().context("附件内容缺失")?)?;
        ensure!(
            !part.is_empty() && offset + part.len() as u64 <= file.size,
            "附件分块大小无效"
        );
        offset += part.len() as u64;
        bytes.extend_from_slice(&part);
        if page["next_offset"].is_null() {
            break;
        }
        ensure!(
            page["next_offset"].as_u64() == Some(offset),
            "附件分块游标无效"
        );
    }
    ensure!(
        bytes.len() as u64 == file.size && format!("{:x}", Sha256::digest(&bytes)) == file.hash,
        "附件摘要不一致"
    );
    Ok(bytes)
}

impl Command {
    pub fn needs_node_operator(&self) -> bool {
        matches!(
            self,
            Self::EditSettings(_)
                | Self::EditOwnerSettings(_)
                | Self::ProviderRead { .. }
                | Self::TestWebhook
                | Self::DecisionTokenRevoke(_)
                | Self::ReadTriage
                | Self::NodeSettings(_)
                | Self::ReadLogs { .. }
                | Self::DiagnosticsSave(_)
        )
    }
}

impl Command {
    pub fn available(&self, snapshot: &Snapshot) -> bool {
        if matches!(self, Self::RestoreProfile { .. }) {
            return snapshot.can_stage_input();
        }
        if matches!(self, Self::ReviewSettings(_)) {
            return snapshot.client_id.is_some();
        }
        if matches!(
            self,
            Self::LoadHistory { .. } | Self::SendStaged(_) | Self::Reconcile(_)
        ) {
            return snapshot.online;
        }
        if matches!(self, Self::StopReply(_) | Self::StopWork { .. }) {
            return snapshot.can_control;
        }
        if matches!(self, Self::AcknowledgeOperatorUnknown(_)) {
            return snapshot.can_configure_owner && snapshot.operator_can_close;
        }
        if matches!(self, Self::EditOwnerSettings(_)) {
            return snapshot.can_configure_owner;
        }
        if self.needs_node_operator() {
            return snapshot.can_configure;
        }
        if matches!(self,Self::Identity(action)if !matches!(action,IdentityAction::Directory)) {
            return snapshot.online && snapshot.can_manage;
        }
        if matches!(
            self,
            Self::DeliveryConfigure { .. }
                | Self::DeliveryConfirm(_)
                | Self::DeliveryAccept(_)
                | Self::DeliveryReconcile(_)
        ) {
            return snapshot.online && snapshot.can_manage;
        }
        true
    }
}

fn operation_label(state: vibeface_client::OperationState) -> &'static str {
    use vibeface_client::OperationState;
    match state {
        OperationState::Prepared => "已存本机，等待发送或前序回执",
        OperationState::Unknown => "服务器结果未知，先查询原回执",
        OperationState::Accepted => "入口已接收，不代表执行完成",
        OperationState::Rejected => "已明确拒绝",
        OperationState::ClosedByUser => "已由你关闭，服务器结果仍未知",
    }
}
#[cfg(test)]
impl Backend {
    /// Infers generator sizes without constructing/polling them or opening any identity.
    pub fn frame_sizes_for_tests() -> Vec<(&'static str, usize)> {
        fn size<F, R>(_: F) -> usize
        where
            F: FnOnce() -> R,
        {
            std::mem::size_of::<R>()
        }
        let absent: Option<&'static mut Backend> = None;
        let run = size(move || absent.expect("type-only probe").run(Command::Refresh));
        let absent: Option<&'static mut Backend> = None;
        let save = size(move || {
            absent
                .expect("type-only probe")
                .save(Editor::Connect, BTreeMap::new(), None)
        });
        let absent: Option<&'static mut Backend> = None;
        let refresh = size(move || absent.expect("type-only probe").refresh());
        let absent: Option<&'static mut Backend> = None;
        let maintain = size(move || absent.expect("type-only probe").maintain());
        let absent: Option<Arc<ClientKey>> = None;
        let client = size(move || {
            Client::open(
                absent.expect("type-only probe"),
                Endpoint::Gateway(Entry {
                    origin: String::new(),
                    node_id: String::new(),
                }),
                PathBuf::new(),
            )
        });
        vec![
            ("backend_run", run),
            ("backend_save", save),
            ("backend_refresh", refresh),
            ("backend_maintain", maintain),
            ("client_open", client),
        ]
    }
}

#[cfg(test)]
impl Backend {
    fn test_phase(phase: &'static str) {
        use std::io::Write as _;
        let _ = writeln!(std::io::stderr(), "native backend phase: {phase}");
    }
}
