//! Bounded shared-history reads. Parse and validate on the client task, before publishing UI state.
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use vibeface_bridge::failure::FailureKind;
use vibeface_protocol::workspace::{Attachment, InputControl, InputIntent, InputTarget, WorkspaceCommand, WorkspaceSpeaker};

/// An event belongs to the connection that rendered its control, even if the network
/// changes before GPUI receives the click. Matching record IDs alone is insufficient.
#[derive(Clone, Debug)]
pub struct RecordScope { pub generation: u64, pub network_id: Option<String> }
impl RecordScope {
    pub fn accepts(&self,generation:u64,network:Option<&str>)->bool{self.generation==generation&&self.network_id.as_deref()==network}
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Provider { Codex, Cursor, Claude, Pi, Opencode }
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum WorkKind { Codex, Cursor, Claude, Pi, Opencode, Task }
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum WorkState { Accepted, Running, Interrupting, Finished, Unverified }
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum WorkPhase { Started, Running, Interrupting, Finishing }
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum WorkOutcome { Completed, Failed, Interrupted, Rejected, Unverified }

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResolvedModel {
    pub provider: Provider,
    #[serde(deserialize_with="nullable_model")] pub model: Option<String>,
}
fn nullable_model<'de,D:serde::Deserializer<'de>>(deserializer:D)->std::result::Result<Option<String>,D::Error>{Option::<String>::deserialize(deserializer)}
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkRecord {
    pub work_id: String, pub conversation_id: String, pub target_node: String,
    pub trigger: String, pub dispatcher: String, pub kind: WorkKind,
    pub cwd: String, pub title: String, pub prompt: String, pub call_ref: String,
    pub model: Option<String>, pub resolved: ResolvedModel,
    pub continues: Option<String>, pub parent: Option<String>, pub thread: Option<String>,
    pub phase: Option<WorkPhase>, pub summary: String,
    pub thread_id: Option<String>, pub turn_id: Option<String>,
    pub outcome: Option<WorkOutcome>, pub result_text: String,
    pub failure_reason: Option<String>, pub failure: Option<FailureKind>, pub state: WorkState,
    pub target: Option<InputTarget>, pub source_events: Vec<String>,
}
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkCoverage { pub source: String, pub complete: bool }
#[derive(Clone, Debug)]
pub struct WorkDetail { pub network_id: String, pub work: WorkRecord, pub coverage: WorkCoverage }

pub fn parse_work_detail(value: Value, network: &str, conversation: &str, work_id: &str) -> Result<WorkDetail> {
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Wire { work: WorkRecord, coverage: WorkCoverage }
    valid_id(network)?; valid_id(conversation)?; valid_id(work_id)?;
    let object=value.get("work").and_then(Value::as_object).context("工作详情缺少工作记录")?;
    // WorkProjection serializes nullable fields explicitly. Missing fields must not be
    // mistaken for a device reporting that the corresponding identity/result is absent.
    for key in ["model","continues","parent","thread","phase","thread_id","turn_id","outcome","failure_reason","failure","target"] {ensure!(object.contains_key(key),"工作详情缺少 {key}");}
    ensure!(object.get("resolved").and_then(Value::as_object).is_some_and(|resolved|resolved.contains_key("model")),"工作详情缺少已接收模型");
    let wire: Wire = serde_json::from_value(value).context("工作详情格式无效")?;
    let work = &wire.work;
    ensure!(work.conversation_id == conversation && work.work_id == work_id, "工作详情不属于请求的对话或工作");
    ensure!(wire.coverage.source == "shared_history" && !wire.coverage.complete, "工作历史范围无效");
    for id in [&work.target_node, &work.trigger, &work.dispatcher, &work.call_ref] { valid_id(id)?; }
    for id in [&work.continues, &work.parent, &work.thread, &work.thread_id, &work.turn_id].into_iter().flatten() { valid_id(id)?; }
    ensure!(work.source_events.len() <= 64, "工作来源超过读取上限");
    ensure!(work.source_events.iter().collect::<BTreeSet<_>>().len()==work.source_events.len(),"工作来源事件重复");
    for source in &work.source_events { valid_id(source)?; }
    if let Some(target) = &work.target { validate_target_ids(target)?; }
    let expected_state=match (work.outcome,work.phase){
        (Some(WorkOutcome::Unverified),_)=>WorkState::Unverified,
        (Some(_),_)=>WorkState::Finished,
        (None,_) if work.kind==WorkKind::Task=>WorkState::Running,
        (None,Some(WorkPhase::Interrupting))=>WorkState::Interrupting,
        (None,Some(_))=>WorkState::Running,
        (None,None)=>WorkState::Accepted,
    };
    ensure!(work.state==expected_state, "工作状态与阶段或结果不一致");
    Ok(WorkDetail { network_id: network.into(), work: wire.work, coverage: wire.coverage })
}

#[derive(Clone, Debug, Default)]
pub struct HistoryBindings {
    pub bot_conversations: BTreeMap<String, String>,
    pub task_channels: BTreeMap<String, Option<String>>,
    pub task_owners: BTreeMap<String, String>,
    pub work_conversations: BTreeMap<String, String>,
    pub channel_coordinators: BTreeMap<String, String>,
}
#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum HistoryScope { Task, Channel, Workspace }
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HistoryCoverage { pub limit: usize, pub truncated: bool, pub scope: HistoryScope }
#[derive(Clone, Copy, Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MessageRole { User, Assistant, Bot }
#[derive(Clone, Copy, Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReplyStatus { Completed, Stopped, Interrupted, Failed }
#[derive(Clone, Debug, Deserialize)]
pub struct HistoryInput {
    pub message_id: String, pub target: InputTarget, pub intent: InputIntent, pub sequence: u64,
    pub expected_revision: Option<u64>, pub text: String,
    #[serde(default)] pub attachments: Vec<Attachment>, pub report_results: Option<bool>,
}
/// Core EventBody has additional signed attestation/causal fields. These remain on the
/// authenticated transport; this projection contains only the fields the native page uses.
#[derive(Clone, Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum HistoryBody {
    InputPosted { #[serde(flatten)] input: HistoryInput },
    WorkspaceCommanded { command_id: String, command: WorkspaceCommand },
    MessagePosted {
        message_id: String, role: MessageRole, text: String,
        #[serde(default)] attachments: Vec<Attachment>,
        in_reply_to: Option<String>, status: Option<ReplyStatus>, failure: Option<FailureKind>,
    },
    WorkDispatched {
        work_id: String, trigger: String, dispatcher: String, call_ref: String, kind: WorkKind,
        model: Option<String>, resolved: ResolvedModel, cwd: String, title: String, prompt: String,
        continues: Option<String>, parent: Option<String>, thread: Option<String>,
    },
    WorkProgress { work_id: String, phase: WorkPhase, summary: String, thread_id: Option<String>, turn_id: Option<String> },
    WorkFinished { work_id: String, outcome: WorkOutcome, result_text: String, failure_reason: Option<String>, failure: Option<FailureKind> },
}
impl HistoryBody {
    pub fn kind(&self) -> &'static str { match self {
        Self::InputPosted {..} => "input_posted", Self::MessagePosted {..} => "message_posted",
        Self::WorkspaceCommanded {..} => "workspace_commanded", Self::WorkDispatched {..} => "work_dispatched",
        Self::WorkProgress {..} => "work_progress", Self::WorkFinished {..} => "work_finished",
    } }
    pub fn attachments(&self) -> &[Attachment] { match self {
        Self::InputPosted {input} => &input.attachments,
        Self::MessagePosted {attachments,..} => attachments,
        Self::WorkspaceCommanded {command: WorkspaceCommand::PostChannelMessage {attachments,..},..} => attachments,
        Self::WorkspaceCommanded {command: WorkspaceCommand::ControlInput {action:InputControl::Edit{attachments,..},..},..} => attachments,
        _ => &[],
    } }
}
#[derive(Clone, Debug)]
pub struct HistoryEvent {
    pub event_hash: String, pub created_at_ms: i64, pub scope_id: String,
    pub speaker: WorkspaceSpeaker, pub body: HistoryBody,
    /// Normalized on the background client task, never by scanning long text in render.
    pub search_text: String,
    /// Exact event scope for attachment reads. Channel command blobs live in `workspace`;
    /// input and reply blobs live in the bot conversation. Never infer it from a recipient.
    pub attachment_conversation: Option<String>,
}
#[derive(Clone, Debug)]
pub struct WorkspaceHistory {
    pub network_id: String, pub channel_id: Option<String>, pub task_id: Option<String>,
    pub events: Vec<HistoryEvent>, pub coverage: HistoryCoverage,
}

pub fn parse_workspace_history(value: Value, network: &str, channel: Option<&str>, task: Option<&str>, bindings: &HistoryBindings) -> Result<WorkspaceHistory> {
    #[derive(Deserialize)]
    struct WireEvent { event_hash: String, created_at_ms: i64, scope_id: String, kind: String, body: HistoryBody, speaker: WorkspaceSpeaker }
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Wire { events: Vec<WireEvent>, coverage: HistoryCoverage }
    valid_id(network)?;
    if let Some(id) = channel { valid_id(id)?; ensure!(bindings.channel_coordinators.contains_key(id), "尚未读取此频道"); }
    if let Some(id) = task {
        valid_id(id)?; ensure!(bindings.task_channels.contains_key(id), "尚未读取此任务");
        if let Some(channel) = channel { ensure!(bindings.task_channels.get(id).and_then(Option::as_deref) == Some(channel), "任务不属于请求的频道"); }
    }
    let wire: Wire = serde_json::from_value(value).context("频道历史格式无效")?;
    let expected = if task.is_some() { HistoryScope::Task } else if channel.is_some() { HistoryScope::Channel } else { HistoryScope::Workspace };
    ensure!(wire.coverage.scope == expected, "历史范围与请求不一致");
    ensure!((1..=512).contains(&wire.coverage.limit) && wire.events.len() <= wire.coverage.limit, "历史条数超过读取范围");
    let mut hashes = BTreeSet::new();
    let allowed_conversations: BTreeSet<_> = bindings.task_owners.iter().filter(|(id,_)| {
        task.is_none_or(|wanted| wanted == id.as_str())
            && channel.is_none_or(|wanted| bindings.task_channels.get(*id).and_then(Option::as_deref) == Some(wanted))
    }).filter_map(|(_,owner)|bindings.bot_conversations.get(owner).cloned()).collect();
    let mut events = Vec::with_capacity(wire.events.len());
    for event in wire.events {
        valid_id(&event.event_hash)?; valid_id(&event.scope_id)?;
        ensure!(hashes.insert(event.event_hash.clone()), "历史包含重复事件");
        ensure!(event.kind == event.body.kind(), "历史事件类型与内容不一致");
        match &event.speaker {
            WorkspaceSpeaker::Human {device_id} | WorkspaceSpeaker::Node {device_id} => valid_id(device_id)?,
            WorkspaceSpeaker::Bot {bot_id} => {
                valid_id(bot_id)?; ensure!(bindings.bot_conversations.contains_key(bot_id), "历史发言 Bot 不在当前网络投影中");
                if event.scope_id!="workspace" {ensure!(bindings.bot_conversations.get(bot_id)==Some(&event.scope_id),"发言 Bot 与历史对话不一致");}
            }
        }
        let conversation = match &event.body {
            HistoryBody::WorkspaceCommanded { command_id, command } => {
                valid_id(command_id)?;
                ensure!(event.scope_id == "workspace", "频道命令不在工作区范围内");
                validate_command_scope(command, channel, task, bindings)?;
                if event.body.attachments().is_empty() { None } else { Some(event.scope_id.clone()) }
            }
            body => {
                ensure!(bindings.bot_conversations.values().any(|id| id == &event.scope_id), "历史属于未读取的对话");
                if channel.is_some() || task.is_some() { ensure!(allowed_conversations.contains(&event.scope_id), "历史对话不属于请求的频道或任务"); }
                if let HistoryBody::InputPosted {input} = body {
                    valid_id(&input.message_id)?; ensure!(input.sequence > 0, "输入序号无效");
                    validate_target_ids(&input.target)?;
                    validate_history_target(&input.target, &event.scope_id, channel, task, bindings)?;
                }
                if let HistoryBody::MessagePosted {message_id, in_reply_to, ..} = body {
                    valid_id(message_id)?; if let Some(id) = in_reply_to { valid_id(id)?; }
                }
                match body {
                    HistoryBody::WorkDispatched {work_id,..} | HistoryBody::WorkProgress {work_id,..} | HistoryBody::WorkFinished {work_id,..} => {
                        valid_id(work_id)?;
                        if let Some(known) = bindings.work_conversations.get(work_id) { ensure!(known == &event.scope_id, "工作历史属于别的对话"); }
                    }
                    _ => {}
                }
                if let HistoryBody::WorkDispatched{trigger,dispatcher,call_ref,continues,parent,thread,..}=body {
                    for id in [trigger,dispatcher,call_ref] {valid_id(id)?;}
                    for id in [continues,parent,thread].into_iter().flatten(){valid_id(id)?;}
                }
                if let HistoryBody::WorkProgress{thread_id,turn_id,..}=body {for id in [thread_id,turn_id].into_iter().flatten(){valid_id(id)?;}}
                Some(event.scope_id.clone())
            }
        };
        validate_attachments(event.body.attachments())?;
        let search_text=history_search_text(&event.body);
        events.push(HistoryEvent {event_hash:event.event_hash,created_at_ms:event.created_at_ms,scope_id:event.scope_id,speaker:event.speaker,body:event.body,search_text,attachment_conversation:conversation});
    }
    Ok(WorkspaceHistory {network_id:network.into(),channel_id:channel.map(str::to_owned),task_id:task.map(str::to_owned),events,coverage:wire.coverage})
}

fn history_search_text(body:&HistoryBody)->String{
    let text=match body{
        HistoryBody::InputPosted{input}=>input.text.clone(),HistoryBody::MessagePosted{text,..}=>text.clone(),
        HistoryBody::WorkDispatched{title,prompt,..}=>format!("{title}\n{prompt}"),HistoryBody::WorkProgress{summary,..}=>summary.clone(),
        HistoryBody::WorkFinished{result_text,failure_reason,..}=>format!("{result_text}\n{}",failure_reason.as_deref().unwrap_or("")),
        HistoryBody::WorkspaceCommanded{command,..}=>match command{
            WorkspaceCommand::PostChannelMessage{text,..}=>text.clone(),
            WorkspaceCommand::CreateTask{title,request,expected_result,..}=>format!("{title}\n{request}\n{expected_result}"),
            WorkspaceCommand::ReviseTask{request,expected_result,..}=>format!("{request}\n{expected_result}"),
            WorkspaceCommand::AssignmentProgress{result,..}=>result.clone().unwrap_or_default(),
            WorkspaceCommand::CreateChannel{title,purpose,..}|WorkspaceCommand::ConfigureChannel{title,purpose,..}=>format!("{title}\n{purpose}"),
            WorkspaceCommand::ControlInput{action:InputControl::Edit{text,..},..}=>text.clone(),_=>String::new(),
        }
    };
    let mut searchable=text.to_lowercase();
    for file in body.attachments(){searchable.push('\n');searchable.push_str(&file.name.to_lowercase());}
    searchable
}

fn validate_history_target(target: &InputTarget, conversation: &str, channel: Option<&str>, task: Option<&str>, bindings: &HistoryBindings) -> Result<()> {
    let (target_task, target_channel, bot) = match target {
        InputTarget::Task {task_id} => (task_id.as_str(), None, bindings.task_owners.get(task_id).map(String::as_str)),
        InputTarget::ChannelTask {channel_id, task_id, bot_id} => (task_id.as_str(), Some(channel_id.as_str()), Some(bot_id.as_str())),
        _ => anyhow::bail!("共享任务历史包含非任务输入"),
    };
    ensure!(task.is_none_or(|id| id == target_task), "历史输入属于其他任务");
    let actual_channel = bindings.task_channels.get(target_task).context("尚未读取历史输入的任务")?.as_deref();
    ensure!(channel.is_none_or(|id| actual_channel == Some(id)), "历史输入属于其他频道");
    ensure!(target_channel.is_none_or(|id| actual_channel == Some(id)), "输入频道与任务不一致");
    let owner = bindings.task_owners.get(target_task).context("尚未读取任务所属 Bot")?;
    ensure!(bot == Some(owner.as_str()) && bindings.bot_conversations.get(owner).map(String::as_str) == Some(conversation), "输入目标与原始对话不一致");
    Ok(())
}

fn validate_command_scope(command: &WorkspaceCommand, channel: Option<&str>, task: Option<&str>, bindings: &HistoryBindings) -> Result<()> {
    if let WorkspaceCommand::PostChannelMessage{message_id,recipient_bot,task_id,reply_to,..}=command {
        valid_id(message_id)?;for id in [recipient_bot,task_id,reply_to].into_iter().flatten(){valid_id(id)?;}
    }
    let (command_channel, command_task) = match command {
        WorkspaceCommand::CreateChannel {channel_id,..} | WorkspaceCommand::ConfigureChannel {channel_id,..} => (Some(channel_id.as_str()), None),
        WorkspaceCommand::PostChannelMessage {channel_id,task_id,attachments,..} => { validate_attachments(attachments)?; (Some(channel_id.as_str()),task_id.as_deref()) },
        WorkspaceCommand::CreateTask {channel_id,task_id,..} => (channel_id.as_deref(),Some(task_id.as_str())),
        WorkspaceCommand::ReviseTask {task_id,..} | WorkspaceCommand::ControlTask {task_id,..} | WorkspaceCommand::AssignTask {task_id,..} => (bindings.task_channels.get(task_id).and_then(Option::as_deref),Some(task_id.as_str())),
        // The Core has already resolved these indirect identities when filtering history.
        WorkspaceCommand::AssignmentProgress {..} | WorkspaceCommand::ControlInput {..} => return Ok(()),
    };
    if let Some(id)=command_channel {valid_id(id)?;}
    if let Some(id)=command_task {valid_id(id)?;}
    ensure!(channel.is_none_or(|id| command_channel == Some(id)), "历史命令属于其他频道");
    if let Some(task) = task { ensure!(command_task == Some(task), "历史命令属于其他任务"); }
    Ok(())
}

pub fn validate_attachments(attachments: &[Attachment]) -> Result<()> {
    ensure!(attachments.len() <= 128, "附件数量超过上限");
    ensure!(attachments.windows(2).all(|pair|pair[0].hash<pair[1].hash), "附件引用必须排序且唯一");
    let mut hashes = BTreeSet::new();
    for file in attachments {
        ensure!(file.hash.len() == 64 && file.hash.bytes().all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c)), "附件哈希无效");
        ensure!(hashes.insert(&file.hash), "附件引用重复");
        ensure!(file.size <= 64 * 1024 * 1024, "附件大小无效");
        ensure!(!file.name.is_empty() && file.name.len() <= 255 && !file.name.chars().any(|c| c == '/' || c == '\\' || c.is_control()), "附件文件名无效");
    }
    Ok(())
}
fn valid_id(id: &str) -> Result<()> { ensure!(!id.is_empty() && id.len() <= 512 && !id.chars().any(char::is_control), "记录标识无效"); Ok(()) }
fn validate_target_ids(target: &InputTarget) -> Result<()> {
    match target {
        InputTarget::BotMain {bot_id} => valid_id(bot_id), InputTarget::Task {task_id} => valid_id(task_id), InputTarget::Work {work_id} => valid_id(work_id),
        InputTarget::Routine {bot_id,trigger} => { valid_id(bot_id)?; valid_id(trigger) },
        InputTarget::ChannelMember {channel_id,bot_id} => { valid_id(channel_id)?; valid_id(bot_id) },
        InputTarget::ChannelTask {channel_id,task_id,bot_id} => { valid_id(channel_id)?; valid_id(task_id)?; valid_id(bot_id) },
    }
}

#[derive(Clone, Debug)]
pub struct ArtifactPage {
    pub network_id: String, pub conversation_id: String, pub file: Attachment,
    pub text: String, pub offset: u64, pub next_offset: Option<u64>,
}
pub fn parse_artifact_page(value: Value, network: &str, conversation: &str, file: &Attachment, offset: u64) -> Result<ArtifactPage> {
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Wire { blob_hash: String, text: String, offset: u64, next_offset: Option<u64>, size: u64 }
    valid_id(network)?; valid_id(conversation)?;
    validate_attachments(std::slice::from_ref(file))?;
    let page: Wire = serde_json::from_value(value).context("附件预览格式无效")?;
    ensure!(page.blob_hash == file.hash && page.size == file.size && page.offset == offset && offset <= file.size, "附件预览与原始引用不一致");
    let end = offset.checked_add(page.text.len() as u64).context("附件游标溢出")?;
    ensure!(end <= file.size && page.text.len() <= 65536, "附件预览超过分块范围");
    ensure!(match page.next_offset { Some(next) => next == end && next > offset && next < file.size, None => end == file.size }, "附件游标未按实际字节推进");
    Ok(ArtifactPage {network_id:network.into(),conversation_id:conversation.into(),file:file.clone(),text:page.text,offset:page.offset,next_offset:page.next_offset})
}
