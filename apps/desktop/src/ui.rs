//! Immutable native view models. Wire parsing runs on the client task, never in render.
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use vibeface_protocol::workspace::{Attachment, InputIntent, InputTarget};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Route {
    Connect,
    Join,
    Home,
    Chat(String),
    History(vibeface_protocol::history::HistorySection),
    Task(String),
    Work {
        conversation_id: String,
        work_id: String,
    },
    Channel(String),
    Inbox,
    Tasks,
    Channels,
    Memory,
    Schedules,
    Bot,
    Devices,
    Settings,
    Node,
    Logs,
    Transfers,
}
impl Route {
    pub fn title(&self) -> &'static str {
        match self {
            Self::Connect => "连接个人网络",
            Self::Join => "加入与恢复",
            Self::Home => "Bot",
            Self::Chat(_) => "对话",
            Self::History(_) => "对话历史",
            Self::Task(_) => "任务详情",
            Self::Work { .. } => "工作详情",
            Self::Channel(_) => "频道",
            Self::Inbox => "待处理",
            Self::Tasks => "任务",
            Self::Channels => "频道",
            Self::Memory => "记忆",
            Self::Schedules => "计划",
            Self::Bot => "Bot 设置",
            Self::Devices => "设备与成员",
            Self::Settings => "设置",
            Self::Node => "本机 Node",
            Self::Logs => "日志",
            Self::Transfers => "文件传输",
        }
    }
}
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct Bot {
    #[serde(default)]
    pub last_message: Option<LastMessage>,
    #[serde(default)]
    pub successor: bool,
    #[serde(default)]
    pub avatar: Option<vibeface_client::Avatar>,
    #[serde(default)]
    pub avatar_version: Option<String>,
    pub bot_id: String,
    pub conversation_id: String,
    pub name: String,
    #[serde(default)]
    pub instructions: String,
    #[serde(default)]
    pub project: Option<Project>,
    #[serde(default)]
    pub models: BTreeMap<String, String>,
    #[serde(default)]
    pub reasoning_efforts: BTreeMap<String, String>,
    #[serde(default)]
    pub skills: Option<Vec<String>>,
    #[serde(default)]
    pub busy: Option<String>,
}
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct Project {
    pub target_node: String,
    pub cwd: String,
}
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct Message {
    #[serde(default)]
    pub sender: Option<MessageSender>,
    #[serde(default)]
    pub target: Option<InputTarget>,
    #[serde(default)]
    pub rank: Option<String>,
    #[serde(default)]
    pub in_reply_to: Option<String>,
    #[serde(default)]
    pub created_at_ms: i64,
    pub message_id: String,
    pub role: String,
    #[serde(default)]
    pub text: String,
    #[serde(default)]
    pub event_hash: String,
    #[serde(default)]
    pub attachments: Vec<Attachment>,
    #[serde(default)]
    pub status: Option<String>,
}
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct Work {
    #[serde(default)]
    pub source_events: Vec<String>,
    #[serde(default)]
    pub thread_id: Option<String>,
    #[serde(default)]
    pub turn_id: Option<String>,
    #[serde(default)]
    pub outcome: Option<crate::views::records::WorkOutcome>,
    pub work_id: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub summary: String,
    #[serde(default)]
    pub state: String,
    #[serde(default)]
    pub result_text: String,
    #[serde(default)]
    pub target_node: String,
}
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct Question {
    #[serde(default)]
    pub about: Option<QuestionAbout>,
    #[serde(default)]
    pub target: Option<InputTarget>,
    #[serde(default)]
    pub closed: Option<String>,
    pub question_id: String,
    #[serde(default)]
    pub conversation_id: String,
    #[serde(default)]
    pub text: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub choices: Vec<Choice>,
    #[serde(default)]
    pub answer: Option<serde_json::Value>,
}
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct Choice {
    pub label: String,
}
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct Task {
    #[serde(default)]
    pub root_task_id: String,
    #[serde(default)]
    pub parent_task_id: Option<String>,
    #[serde(default)]
    pub source_events: Vec<String>,
    #[serde(default)]
    pub waiting_reason: Option<String>,
    #[serde(default)]
    pub current_assignment: Option<String>,
    #[serde(default)]
    pub channel_id: Option<String>,
    pub task_id: String,
    pub title: String,
    pub owner_bot: String,
    #[serde(default)]
    pub request: String,
    #[serde(default)]
    pub expected_result: String,
    #[serde(default)]
    pub dependencies: Vec<String>,
    pub revision: u64,
    #[serde(default)]
    pub state: String,
}
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct Channel {
    pub channel_id: String,
    pub title: String,
    pub purpose: String,
    pub members: Vec<String>,
    pub coordinator_bot: String,
    pub coordinator_node: String,
    pub revision: u64,
    pub owner_epoch: u64,
}
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct Node {
    pub device_id: String,
    pub name: String,
    #[serde(default)]
    pub online: bool,
    #[serde(default)]
    pub revoked: bool,
    #[serde(default)]
    pub local: bool,
}
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct Member {
    pub device_id: String,
    pub name: String,
    pub kind: String,
    #[serde(default)]
    pub manage: bool,
}
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct Memory {
    pub memory_id: String,
    pub text: String,
    pub metadata: MemoryMetadata,
}
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct MemoryMetadata {
    pub epoch: u64,
    pub revision: String,
    pub control_revision: u64,
    pub source_event_hash: String,
    #[serde(default)]
    pub user_controlled: bool,
}
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct Schedule {
    pub schedule_id: String,
    pub name: String,
    pub prompt: String,
    pub enabled: bool,
    pub first_run_at_ms: i64,
    #[serde(default)]
    pub every_minutes: Option<u64>,
    #[serde(default)]
    pub next_run_at_ms: Option<i64>,
}
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct Scheduler {
    #[serde(default)]
    pub unanswered: Vec<String>,
    #[serde(default)]
    pub remote: Option<RemoteScheduler>,
    #[serde(default)]
    pub jobs: Vec<Job>,
    #[serde(default)]
    pub scope_jobs: Vec<ScopeJob>,
}
impl Scheduler {
    pub fn retry_job_for_message(&self, message_id: &str) -> Option<&Job> {
        let jobs = match self
            .remote
            .as_ref()
            .filter(|remote| !remote.owner.is_empty())
        {
            Some(remote) if remote.reachable => &remote.jobs,
            Some(_) => return None,
            None => &self.jobs,
        };
        jobs.iter()
            .find(|job| job.message_id.as_deref() == Some(message_id))
    }
    pub fn effective_jobs(&self) -> Vec<&Job> {
        let remote = self.remote.as_ref().filter(|remote| remote.reachable);
        let mut seen = std::collections::HashSet::new();
        remote
            .into_iter()
            .flat_map(|remote| remote.jobs.iter())
            .chain(self.jobs.iter())
            .filter(|job| seen.insert(job.trigger.as_str()))
            .collect()
    }
    pub fn effective_scope_jobs(&self) -> Vec<&ScopeJob> {
        let remote = self.remote.as_ref().filter(|remote| remote.reachable);
        let mut seen = std::collections::HashSet::new();
        remote
            .into_iter()
            .flat_map(|remote| remote.scope_jobs.iter())
            .chain(self.scope_jobs.iter())
            .filter(|job| seen.insert(job.trigger.as_str()))
            .collect()
    }
}
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct Job {
    #[serde(default)]
    pub retryable: bool,
    pub trigger: String,
    pub state: String,
    pub scope: Scope,
    #[serde(default)]
    pub message_id: Option<String>,
    #[serde(default)]
    pub failure: Option<vibeface_bridge::failure::ExecFailure>,
}
impl Job {
    pub fn is_active(&self) -> bool {
        [
            "queued",
            "claimed",
            "running",
            "steering",
            "cancelling",
            "reconciling",
        ]
        .contains(&self.state.as_str())
    }
    pub fn can_retry(&self) -> bool {
        if !self.retryable
            || !["failed", "rejected", "cancelled", "interrupted"].contains(&self.state.as_str())
        {
            return false;
        }
        self.failure.as_ref().is_none_or(|failure| {
            use vibeface_bridge::failure::FailureKind;
            !matches!(
                failure.kind,
                FailureKind::Transport
                    | FailureKind::TimedOut
                    | FailureKind::Restarted
                    | FailureKind::Lost
                    | FailureKind::Unknown
            ) && !failure
                .detail
                .to_ascii_lowercase()
                .contains("outcome may be unknown")
                && !failure.detail.to_ascii_lowercase().contains("timed out")
        })
    }
}
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Scope {
    #[default]
    Main,
    WorkspaceTask {
        task_id: String,
    },
    Task {
        work_id: String,
    },
    Routine {
        trigger: String,
    },
}
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct ScopeJob {
    pub conversation_id: String,
    pub trigger: String,
    pub scope: Scope,
    #[serde(default)]
    pub state: String,
    #[serde(default)]
    pub failure: Option<vibeface_bridge::failure::ExecFailure>,
}
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct ConversationIdentity {
    pub conversation_id: String,
    #[serde(default)]
    pub owner: Option<String>,
    pub owner_epoch: u64,
}
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct Conversation {
    #[serde(default)]
    pub history_authority: String,
    #[serde(default)]
    pub history: BTreeMap<String, vibeface_protocol::history::HistoryCoverage>,
    #[serde(default)]
    pub changes: Vec<crate::views::history::BotChangeRecord>,
    #[serde(default)]
    pub overwritten: Vec<crate::views::history::OverwrittenRecord>,
    #[serde(default)]
    pub deliveries: Vec<DeliveryControl>,
    #[serde(default)]
    pub sessions: Vec<SessionHandoff>,
    #[serde(default)]
    pub checks: Vec<CoordinationCheck>,
    #[serde(default)]
    pub conversation: Option<ConversationIdentity>,
    #[serde(default, deserialize_with = "deserialize_scheduler")]
    pub scheduler: Scheduler,
    #[serde(default)]
    pub messages: Vec<Message>,
    #[serde(default)]
    pub works: Vec<Work>,
    #[serde(default)]
    pub questions: Vec<Question>,
    #[serde(default)]
    pub bot_settings: BotSettings,
}
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct BotSettings {
    #[serde(default)]
    pub skills: Vec<Skill>,
    #[serde(default)]
    pub memory_sync_conflicts: MemorySyncConflicts,
    #[serde(default)]
    pub memory_staged_count: Option<u64>,
    #[serde(default)]
    pub memory_conflicts: Option<Vec<MemoryConflict>>,
    #[serde(default)]
    pub memory: Vec<Memory>,
    #[serde(default)]
    pub schedules: Vec<Schedule>,
}
#[derive(Clone, Debug, Default)]
pub struct Snapshot {
    pub identity_busy: bool,
    pub profiles: Vec<crate::host::profiles::PersonalProfile>,
    pub profile_id: Option<String>,
    pub credential_scope: Option<String>,
    pub generation: u64,
    pub online: bool,
    pub synced: bool,
    pub private_block: Option<String>,
    pub projection_valid: bool,
    pub cached: bool,
    pub cached_at_ms: Option<u64>,
    pub cache_omitted: usize,
    pub writable: bool,
    pub can_save_locally: bool,
    pub can_control: bool,
    pub local_session: bool,
    pub write_state: String,
    pub network: Option<String>,
    pub client_id: Option<String>,
    pub can_manage: bool,
    pub can_configure: bool,
    pub can_configure_owner: bool,
    pub operator_unknown: Option<String>,
    pub operator_unknown_kind: Option<String>,
    pub operator_can_close: bool,
    pub operator_fields: Vec<String>,
    pub operator_secret_changes: bool,
    pub decision_tokens: Vec<DecisionTokenSummary>,
    pub bots: Vec<Bot>,
    pub nodes: Vec<Node>,
    pub members: Vec<Member>,
    pub joins: Vec<JoinRequest>,
    pub inputs: Vec<vibeface_protocol::workspace::QueuedInput>,
    pub tasks: Vec<Task>,
    pub assignments: Vec<vibeface_protocol::workspace::Assignment>,
    pub channels: Vec<Channel>,
    pub conversation: std::sync::Arc<Conversation>,
    pub work_detail: Option<std::sync::Arc<crate::views::records::WorkDetail>>,
    pub channel_history: Option<std::sync::Arc<crate::views::records::WorkspaceHistory>>,
    pub artifact_preview: Option<std::sync::Arc<crate::views::records::ArtifactPage>>,
    pub settings_review: Option<SettingsReviewView>,
    pub pending: Vec<Pending>,
    pub identity_plan: Option<IdentityPlanView>,
    pub drafts: Vec<NativeDraftSummary>,
    pub questions: Vec<Question>,
    pub notifications: Vec<Notification>,
    pub history_loading: Option<vibeface_protocol::history::HistorySection>,
    pub history_error: Option<String>,
    pub live: Vec<LiveProgress>,
    pub delivery: Delivery,
    pub status: String,
    pub error: Option<String>,
}
#[derive(Clone, Debug)]
pub struct Pending {
    pub id: String,
    pub state: String,
    pub target_node: String,
}
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
pub struct ComposerDraft {
    pub target: InputTarget,
    pub intent: InputIntent,
    pub text: String,
    pub attachments: Vec<Attachment>,
    pub expected_revision: Option<u64>,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ComposerOwner {
    pub token: String,
    pub generation: u64,
    pub profile_id: Option<String>,
    pub network: Option<String>,
    pub client_id: Option<String>,
    pub route: Route,
    pub view_conversation: Option<String>,
    pub editor_revision: u64,
    pub target_kind: usize,
    pub selected_target: Option<InputTarget>,
    pub target_revision: Option<u64>,
    pub attachment_scope: Option<String>,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InputAck {
    pub owner: Option<ComposerOwner>,
    pub conversation_id: String,
    pub draft: ComposerDraft,
}
impl ComposerDraft {
    pub fn validate(&self) -> Result<(), String> {
        if self.text.trim().is_empty() && self.attachments.is_empty() {
            return Err("请输入消息或添加附件".into());
        }
        if self.text.len() > 4 * 1024 * 1024 {
            return Err("消息超过 4 MiB，请作为附件发送".into());
        }
        Ok(())
    }
}
pub fn decode_list<T: serde::de::DeserializeOwned>(
    value: &serde_json::Value,
    key: &str,
) -> anyhow::Result<Vec<T>> {
    let array = if value.is_array() {
        value.clone()
    } else {
        value
            .get(key)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("响应缺少 {key}"))?
    };
    Ok(serde_json::from_value(array)?)
}

fn deserialize_scheduler<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Scheduler, D::Error> {
    Ok(Option::<Scheduler>::deserialize(d)?.unwrap_or_default())
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct Delivery {
    #[serde(default)]
    pub node_peer: String,
    #[serde(default)]
    pub wallet: Wallet,
    #[serde(default)]
    pub receiver_key: Option<ReceiverKey>,
    #[serde(default)]
    pub providers: Vec<TransferProvider>,
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub charity_enabled: bool,
    #[serde(default)]
    pub paid_enabled: bool,
    #[serde(default)]
    pub max_file_bytes: u64,
    #[serde(default)]
    pub orders: Vec<TransferOrder>,
    #[serde(default)]
    pub budgets: Vec<TransferBudget>,
    #[serde(default)]
    pub budget_error: Option<String>,
}
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct TransferOrder {
    pub order_id: String,
    pub intent_id: String,
    pub role: String,
    pub mode: String,
    pub acceptance: String,
    pub source_peer: String,
    pub receiver_peer: String,
    pub provider_peer: String,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub bytes: u64,
    #[serde(default)]
    pub sha256: Option<String>,
    pub phase: String,
    #[serde(default)]
    pub evidence: String,
    #[serde(default)]
    pub deadlines: Option<TransferDeadlines>,
    #[serde(default)]
    pub windows: Vec<TransferWindow>,
    #[serde(default)]
    pub payment: Payment,
    #[serde(default)]
    pub error: Option<String>,
}
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq)]
pub struct Payment {
    #[serde(default)]
    pub budget_id: Option<String>,
    #[serde(default)]
    pub settlement: Option<TransferSettlement>,
    #[serde(default)]
    pub chain_id: Option<String>,
    #[serde(default)]
    pub asset: Option<String>,
    #[serde(default)]
    pub delivery_fee: String,
    #[serde(default)]
    pub reservation_fee: String,
    #[serde(default)]
    pub priority_fee: String,
    #[serde(default)]
    pub earned: String,
    #[serde(default)]
    pub withdrawn: String,
    #[serde(default)]
    pub refunded: String,
    #[serde(default)]
    pub state: String,
    #[serde(default)]
    pub total: String,
    #[serde(default)]
    pub gas_cap: String,
    #[serde(default)]
    pub payer: Option<String>,
    #[serde(default)]
    pub provider_wallet: Option<String>,
    #[serde(default)]
    pub tx_hash: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct JoinRequest {
    pub join_id: String,
    pub name: String,
    pub device_id: String,
    #[serde(default)]
    pub code: Option<String>,
}

pub fn parse_conversation(
    value: serde_json::Value,
    expected_id: &str,
    bot_id: Option<&str>,
) -> anyhow::Result<Conversation> {
    let mut conversation: Conversation = serde_json::from_value(value)?;
    anyhow::ensure!(
        conversation
            .conversation
            .as_ref()
            .map(|c| c.conversation_id.as_str())
            == Some(expected_id),
        "设备返回了别的对话，已暂停"
    );
    anyhow::ensure!(
        conversation
            .scheduler
            .scope_jobs
            .iter()
            .all(|job| job.conversation_id == expected_id),
        "目标任务观察属于别的对话"
    );
    if let Some(remote) = &conversation.scheduler.remote {
        anyhow::ensure!(
            remote
                .scope_jobs
                .iter()
                .all(|job| job.conversation_id == expected_id),
            "拥有者任务观察属于别的对话"
        );
    }
    anyhow::ensure!(
        conversation
            .questions
            .iter()
            .all(|q| q.conversation_id == expected_id),
        "问题属于别的对话"
    );
    for message in &conversation.messages {
        crate::views::records::validate_attachments(&message.attachments)?;
        anyhow::ensure!(!message.message_id.is_empty(), "消息缺少稳定标识");
        if let (Some(InputTarget::BotMain { bot_id: target }), Some(expected)) =
            (&message.target, bot_id)
        {
            anyhow::ensure!(target == expected, "消息指向其他 Bot");
        }
    }
    conversation
        .scheduler
        .jobs
        .retain(|job| matches!(job.scope, Scope::Main));
    conversation.messages.retain(|message| {
        message
            .target
            .as_ref()
            .is_none_or(|target| matches!(target, InputTarget::BotMain { .. }))
    });
    conversation
        .messages
        .sort_by(|a, b| match (&a.rank, &b.rank) {
            (Some(a), Some(b)) => a.cmp(b),
            (Some(_), None) => std::cmp::Ordering::Less,
            (None, Some(_)) => std::cmp::Ordering::Greater,
            _ => std::cmp::Ordering::Equal,
        });
    conversation
        .questions
        .retain(|q| matches!(q.target, Some(InputTarget::BotMain { .. })));
    Ok(conversation)
}
#[derive(Clone, Debug, Default)]
pub struct LiveProgress {
    pub key: String,
    pub conversation_id: String,
    pub sequence: u64,
    pub text: String,
    pub status: String,
    pub trigger: Option<String>,
    pub work_id: Option<String>,
    pub invocation: Option<String>,
    pub items: Vec<(String, String)>,
    pub tools: Vec<ToolProgress>,
    pub tools_omitted: usize,
}
pub fn apply_progress(
    streams: &mut Vec<LiveProgress>,
    event: &serde_json::Value,
) -> anyhow::Result<bool> {
    let kind = event["type"].as_str().unwrap_or("");
    anyhow::ensure!(
        ["manager_progress", "native_progress"].contains(&kind),
        "未知进度类型"
    );
    let field = |key: &str| {
        event[key]
            .as_str()
            .filter(|s| !s.is_empty())
            .ok_or_else(|| anyhow::anyhow!("进度缺少 {key}"))
    };
    let conversation = field("conversation_id")?;
    let key = if kind == "manager_progress" {
        format!(
            "manager:{conversation}:{}:{}",
            field("source_node")?,
            field("trigger")?
        )
    } else {
        format!(
            "native:{conversation}:{}:{}",
            field("target_node")?,
            field("work_id")?
        )
    };
    let sequence = event["sequence"]
        .as_u64()
        .filter(|seq| *seq > 0)
        .ok_or_else(|| anyhow::anyhow!("进度序号无效"))?;
    let invocation = event["invocation_id"].as_str().map(str::to_owned);
    if !streams.iter().any(|s| s.key == key) {
        streams.push(LiveProgress {
            key: key.clone(),
            conversation_id: conversation.into(),
            ..Default::default()
        });
    }
    let current = streams
        .iter_mut()
        .find(|s| s.key == key)
        .expect("inserted stream");
    if current.invocation != invocation {
        *current = LiveProgress {
            key,
            conversation_id: conversation.into(),
            invocation: invocation.clone(),
            ..Default::default()
        };
    }
    current.trigger = event["trigger"].as_str().map(str::to_owned);
    current.work_id = event["work_id"].as_str().map(str::to_owned);
    if sequence <= current.sequence {
        return Ok(false);
    }
    let gap = sequence > current.sequence + 1;
    let progress = &event["progress"];
    match progress["type"].as_str() {
        Some("text_snapshot" | "text_delta") => {
            let text = progress["text"]
                .as_str()
                .ok_or_else(|| anyhow::anyhow!("进度文本无效"))?;
            let item_id = progress["item_id"].as_str().unwrap_or("message");
            if !current.items.iter().any(|(id, _)| id == item_id) {
                current.items.push((item_id.into(), String::new()));
            }
            let item = current
                .items
                .iter_mut()
                .find(|(id, _)| id == item_id)
                .expect("inserted item");
            if progress["type"] == "text_snapshot" {
                item.1 = text.into()
            } else if !gap {
                item.1.push_str(text)
            }
            current.text = current
                .items
                .iter()
                .map(|(_, text)| text.as_str())
                .collect::<Vec<_>>()
                .join("\n\n");
        }
        Some("status" | "tool") => {
            current.status = progress["text"]
                .as_str()
                .or_else(|| progress["status"].as_str())
                .or_else(|| progress["name"].as_str())
                .unwrap_or("状态待确认")
                .into();
            let item_id = progress["item_id"]
                .as_str()
                .map(str::to_owned)
                .unwrap_or_else(|| format!("tool-{sequence}"));
            let tool = ToolProgress {
                item_id: item_id.clone(),
                name: progress["name"].as_str().unwrap_or("").into(),
                status: progress["status"].as_str().unwrap_or("").into(),
                text: progress["text"].as_str().unwrap_or("").into(),
                sequence,
            };
            if let Some(existing) = current
                .tools
                .iter_mut()
                .find(|tool| tool.item_id == item_id)
            {
                *existing = tool
            } else {
                current.tools.push(tool)
            }
            if current.tools.len() > 128 {
                current.tools.remove(0);
                current.tools_omitted += 1;
            }
        }
        _ => anyhow::bail!("进度内容无效"),
    };
    current.sequence = sequence;
    Ok(gap)
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct MemorySyncConflicts {
    #[serde(default)]
    pub items: Vec<MemorySyncConflict>,
    #[serde(default)]
    pub has_more: bool,
}
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct MemorySyncConflict {
    pub memory_id: String,
    pub losing_revision: String,
    pub winning_revision: String,
    pub reason: String,
}
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct MemoryConflict {
    #[serde(default)]
    pub history_id: String,
    #[serde(default)]
    pub call_ref: String,
    pub memory_id: String,
    pub expected_revision: String,
    pub actual_revision: String,
    pub reason: String,
}

pub fn target_conversation(
    target: &InputTarget,
    shown: &Snapshot,
    current: Option<&str>,
) -> anyhow::Result<String> {
    let owner = match target {
        InputTarget::BotMain { bot_id }
        | InputTarget::Routine { bot_id, .. }
        | InputTarget::ChannelMember { bot_id, .. }
        | InputTarget::ChannelTask { bot_id, .. } => Some(bot_id.as_str()),
        InputTarget::Task { task_id } => Some(
            shown
                .tasks
                .iter()
                .find(|t| &t.task_id == task_id)
                .ok_or_else(|| anyhow::anyhow!("目标任务未加载"))?
                .owner_bot
                .as_str(),
        ),
        InputTarget::Work { work_id } => {
            anyhow::ensure!(
                shown
                    .conversation
                    .works
                    .iter()
                    .any(|work| &work.work_id == work_id),
                "目标工作不属于当前对话"
            );
            return current
                .map(str::to_owned)
                .ok_or_else(|| anyhow::anyhow!("工作对话未选择"));
        }
    };
    let bot = shown
        .bots
        .iter()
        .find(|bot| Some(bot.bot_id.as_str()) == owner)
        .ok_or_else(|| anyhow::anyhow!("目标 Bot 未加载"))?;
    Ok(bot.conversation_id.clone())
}

#[derive(Clone, Debug)]
pub struct NativeDraftSummary {
    pub fingerprint: String,
    pub text: String,
    pub conversation_id: String,
}
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct CoordinationCheck {
    pub check_id: String,
    pub conversation_id: String,
    pub trigger: String,
    pub attempt_id: String,
    pub detail: String,
}
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct Notification {
    pub notification_id: String,
    pub kind: String,
    pub text: String,
    pub conversation_id: String,
    #[serde(default)]
    pub target: Option<InputTarget>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct Skill {
    pub skill_id: String,
    pub name: String,
    pub description: String,
    #[serde(default)]
    pub body: String,
}

#[derive(Clone, Debug)]
pub struct IdentityPlanView {
    pub backup_path: String,
    pub sha256: String,
    pub network_id: String,
    pub state: String,
}

pub fn prune_terminal_progress(streams: &mut Vec<LiveProgress>, conversation: &Conversation) {
    streams.retain(|progress| {
        if let Some(work_id) = &progress.work_id
            && let Some(work) = conversation
                .works
                .iter()
                .find(|work| &work.work_id == work_id)
        {
            return !["finished", "completed", "failed", "interrupted", "rejected"]
                .contains(&work.state.as_str());
        }
        if let Some(trigger) = &progress.trigger {
            if conversation.messages.iter().any(|message| {
                message.role == "assistant"
                    && message.in_reply_to.as_ref() == Some(trigger)
                    && message.status.is_some()
            }) {
                return false;
            }
            if let Some(job) = conversation
                .scheduler
                .effective_jobs()
                .into_iter()
                .find(|job| &job.trigger == trigger)
            {
                return ![
                    "finished",
                    "completed",
                    "failed",
                    "cancelled",
                    "interrupted",
                    "stopped",
                    "answered",
                    "steered",
                ]
                .contains(&job.state.as_str());
            }
        }
        true
    });
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct SessionHandoff {
    #[serde(default)]
    pub rank: Option<String>,
    pub node: String,
    pub agent: crate::views::records::Provider,
    pub session_id: String,
    pub cwd: String,
    pub summary: String,
    pub muted: bool,
    pub created_at_ms: i64,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq)]
pub struct TransferDeadlines {
    pub quote_expires_at: u64,
    pub hold_deadline: u64,
    pub key_deadline: u64,
    pub open_deadline: u64,
    pub retain_until: u64,
    pub priority_deadline: u64,
}
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct TransferWindow {
    pub index: u64,
    pub bytes: u64,
    pub committed_bytes: u64,
    pub state: String,
    pub decryption_proven: bool,
}
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct Wallet {
    pub state: String,
    #[serde(default)]
    pub address: Option<String>,
    #[serde(default)]
    pub reason: Option<String>,
}
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct ReceiverKey {
    pub key_id: String,
    pub serial: u64,
}
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct TransferProvider {
    pub device_id: String,
    pub paid: bool,
    #[serde(default)]
    pub offer: Option<TransferOffer>,
}
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct TransferOffer {
    pub price: String,
    #[serde(default)]
    pub settlement: Option<TransferOfferSettlement>,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct TransferOfferSettlement {
    pub chain_id: u64,
    pub escrow: String,
    pub asset: String,
}
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct TransferSettlement {
    pub earned: String,
    pub withdrawn: String,
    pub refunded: String,
    pub publication: Option<TransferPublication>,
}
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct TransferPublication {
    pub state: TransferPublicationState,
    pub total_amount: String,
    pub economic_cutoff: u64,
    pub availability_risk: Option<String>,
}
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TransferPublicationState {
    PrivateReady,
    DisclosedOutstanding,
    FinalEarned,
    FinalSettled,
    ExpiredUnrevealed,
}
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct TransferReadyWindow {
    pub order_id: String,
    pub index: u16,
}
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct TransferBudget {
    pub budget_id: String,
    pub payer: String,
    pub payee: String,
    pub provider_peer: String,
    pub asset: String,
    pub deposit: String,
    pub free: String,
    pub locked: String,
    pub earned: String,
    pub withdrawn: String,
    pub refunded: String,
    pub state: String,
    pub gas_cap: String,
    pub ready_windows: Vec<TransferReadyWindow>,
}
#[derive(Clone, Debug)]
pub struct TransferFlush {
    pub budget: TransferBudget,
    pub orders: Vec<TransferOrder>,
}
pub fn validate_transfer_order(order: &TransferOrder) -> anyhow::Result<()> {
    anyhow::ensure!(
        ["sender", "receiver", "provider"].contains(&order.role.as_str())
            && ["charity", "paid"].contains(&order.mode.as_str())
            && ["recoverable", "strict"].contains(&order.acceptance.as_str()),
        "传输角色或条款无效"
    );
    anyhow::ensure!(
        transfer_peer_valid(&order.order_id)
            && [
                &order.source_peer,
                &order.receiver_peer,
                &order.provider_peer
            ]
            .iter()
            .all(|peer| transfer_peer_valid(peer))
            && uuid::Uuid::parse_str(&order.intent_id)
                .is_ok_and(|id| id.to_string() == order.intent_id),
        "订单标识无效"
    );
    anyhow::ensure!(
        order.windows.len() <= 16
            && order.bytes <= 16 * 65536
            && order
                .windows
                .iter()
                .enumerate()
                .all(|(index, window)| window.index == index as u64
                    && window.bytes <= 65536
                    && window.committed_bytes <= window.bytes + 68),
        "订单片段或大小无效"
    );
    for amount in [
        &order.payment.delivery_fee,
        &order.payment.reservation_fee,
        &order.payment.priority_fee,
        &order.payment.total,
        &order.payment.gas_cap,
        &order.payment.earned,
        &order.payment.withdrawn,
        &order.payment.refunded,
    ] {
        anyhow::ensure!(transfer_amount_valid(amount), "订单金额无效");
    }
    anyhow::ensure!(
        transfer_sum([
            order.payment.delivery_fee.as_str(),
            order.payment.reservation_fee.as_str(),
            order.payment.priority_fee.as_str()
        ])? == order.payment.total
            && transfer_amount_at_most(&order.payment.withdrawn, &order.payment.earned)
            && transfer_amount_at_most(
                &transfer_sum([
                    order.payment.earned.as_str(),
                    order.payment.refunded.as_str()
                ])?,
                &order.payment.total
            ),
        "订单金额不守恒"
    );
    if let Some(settlement) = &order.payment.settlement {
        anyhow::ensure!(
            [
                &settlement.earned,
                &settlement.withdrawn,
                &settlement.refunded
            ]
            .iter()
            .all(|value| transfer_amount_valid(value))
                && transfer_amount_at_most(&settlement.withdrawn, &settlement.earned),
            "预算结算金额无效"
        );
        if let Some(publication) = &settlement.publication {
            anyhow::ensure!(
                transfer_amount_valid(&publication.total_amount),
                "批次公开状态无效"
            );
        }
    }
    if order.role == "provider" {
        anyhow::ensure!(
            order.name.is_none() && order.sha256.is_none(),
            "服务商视图包含了明文文件信息"
        );
    }
    if order.mode == "charity" {
        anyhow::ensure!(
            order.acceptance == "recoverable"
                && order.payment.state == "free"
                && [
                    &order.payment.delivery_fee,
                    &order.payment.reservation_fee,
                    &order.payment.priority_fee,
                    &order.payment.total,
                    &order.payment.gas_cap,
                    &order.payment.earned,
                    &order.payment.withdrawn,
                    &order.payment.refunded
                ]
                .iter()
                .all(|value| value.as_str() == "0")
                && order.payment.chain_id.is_none()
                && order.payment.asset.is_none()
                && order.payment.payer.is_none()
                && order.payment.provider_wallet.is_none()
                && order.payment.tx_hash.is_none()
                && order.payment.budget_id.is_none()
                && order.payment.settlement.is_none(),
            "免费订单包含付款条件"
        );
    } else {
        anyhow::ensure!(
            order.payment.state != "free"
                && order
                    .payment
                    .chain_id
                    .as_deref()
                    .is_some_and(transfer_amount_valid)
                && order.payment.asset.is_some()
                && order.payment.payer.is_some()
                && order.payment.provider_wallet.is_some(),
            "付费订单缺少固定付款信息"
        );
        anyhow::ensure!(
            [
                order.payment.asset.as_deref(),
                order.payment.payer.as_deref(),
                order.payment.provider_wallet.as_deref()
            ]
            .into_iter()
            .all(|value| value.is_some_and(transfer_address_valid)),
            "付费订单固定地址无效"
        );
        if order.acceptance == "recoverable" {
            anyhow::ensure!(
                order
                    .payment
                    .budget_id
                    .as_deref()
                    .is_some_and(transfer_peer_valid),
                "受保护订单缺少固定预付预算"
            );
        }
        let d = order
            .deadlines
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("付费订单缺少截止时间"))?;
        anyhow::ensure!(
            d.quote_expires_at > 0
                && d.quote_expires_at <= d.hold_deadline
                && d.hold_deadline < d.key_deadline
                && d.key_deadline <= d.open_deadline
                && d.open_deadline <= d.retain_until
                && d.priority_deadline > d.quote_expires_at
                && d.priority_deadline
                    <= if order.acceptance == "strict" {
                        d.open_deadline
                    } else {
                        d.key_deadline
                    },
            "订单报价、交付或时效截止无效"
        );
    }
    Ok(())
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct DecisionTokenSummary {
    pub token_id: String,
    pub label: String,
    pub created_ms: i64,
}

#[derive(Clone, Debug, Default)]
pub struct ToolProgress {
    pub item_id: String,
    pub name: String,
    pub status: String,
    pub text: String,
    pub sequence: u64,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct QuestionAbout {
    pub node: String,
    pub session_id: String,
    #[serde(default)]
    pub request_id: Option<String>,
}

pub fn can_read_transfer(order: &TransferOrder) -> bool {
    order.mode == "charity" && order.role == "sender"
        || order.role == "receiver"
            && ["charity_receipt", "recoverable", "decryption_proven"]
                .contains(&order.evidence.as_str())
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct LastMessage {
    pub text: String,
    pub role: String,
    pub created_at_ms: i64,
}
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct MessageSender {
    pub name: String,
    pub conversation_id: String,
}

#[derive(Clone, Debug)]
pub struct SettingsReviewView {
    pub review: vibeface_client::SettingsReview,
    pub current: Vec<vibeface_client::SettingsField>,
    pub original_rows: Vec<(String, String)>,
    pub read_error: Option<String>,
}

impl Snapshot {
    pub fn can_stage_input(&self) -> bool {
        self.can_save_locally && (self.synced || self.cached || self.projection_valid)
    }
}
/// A remembered origin and Node are independent defaults. A typed Node must never be ignored.
pub fn connection_entry(
    origin: &str,
    node_id: &str,
    remembered: Option<&vibeface_client::Entry>,
    gateway_required: bool,
) -> anyhow::Result<vibeface_client::Entry> {
    let origin = if origin.trim().is_empty() {
        remembered
            .map(|entry| entry.origin.clone())
            .unwrap_or_default()
    } else {
        origin.trim().to_owned()
    };
    let node_id = if node_id.trim().is_empty() {
        remembered
            .map(|entry| entry.node_id.clone())
            .unwrap_or_default()
    } else {
        node_id.trim().to_owned()
    };
    anyhow::ensure!(
        node_id.len() == 64
            && node_id
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)),
        "目标Node须为完整的64位小写十六进制公钥"
    );
    if gateway_required {
        let url =
            reqwest::Url::parse(&origin).map_err(|_| anyhow::anyhow!("请提供有效Gateway地址"))?;
        anyhow::ensure!(
            matches!(url.scheme(), "http" | "https")
                && url.host_str().is_some()
                && url.username().is_empty()
                && url.password().is_none()
                && url.query().is_none()
                && url.fragment().is_none(),
            "Gateway地址仅接受无凭证、查询或片段的HTTP(S)入口"
        );
    }
    Ok(vibeface_client::Entry { origin, node_id })
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct RemoteScheduler {
    #[serde(default)]
    pub owner: String,
    #[serde(default)]
    pub reachable: bool,
    #[serde(default)]
    pub jobs: Vec<Job>,
    #[serde(default)]
    pub scope_jobs: Vec<ScopeJob>,
    #[serde(default)]
    pub memory_conflicts: Option<Vec<MemoryConflict>>,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum DeliveryControl {
    StopWaiting {
        work_id: String,
        target_node: String,
    },
    StopTimedOut {
        work_id: String,
        target_node: String,
    },
    DispatchWaiting {
        work_id: String,
        target_node: String,
        cwd: String,
        #[serde(default)]
        trigger: Option<String>,
        #[serde(default)]
        title: Option<String>,
        #[serde(default)]
        prompt: Option<String>,
        #[serde(default)]
        reason: Option<String>,
    },
    DispatchRejected {
        work_id: String,
        target_node: String,
        cwd: String,
        #[serde(default)]
        trigger: Option<String>,
        #[serde(default)]
        title: Option<String>,
        #[serde(default)]
        prompt: Option<String>,
        #[serde(default)]
        reason: Option<String>,
    },
    DispatchUnverified {
        work_id: String,
        target_node: String,
        cwd: String,
        #[serde(default)]
        trigger: Option<String>,
        #[serde(default)]
        title: Option<String>,
        #[serde(default)]
        prompt: Option<String>,
        #[serde(default)]
        reason: Option<String>,
    },
}

/// User supplied targets never inherit direct hints for another Node.
pub fn connection_endpoint(
    transport: &str,
    origin: &str,
    node_id: &str,
    addresses: &str,
    remembered: Option<&vibeface_client::Endpoint>,
    hint: Option<&vibeface_client::Entry>,
) -> anyhow::Result<vibeface_client::Endpoint> {
    use vibeface_client::{Endpoint, Entry};
    anyhow::ensure!(
        ["", "auto", "gateway", "lan"].contains(&transport),
        "接入方式须为 auto、gateway 或 lan"
    );
    if transport.is_empty()
        && origin.trim().is_empty()
        && node_id.trim().is_empty()
        && addresses.trim().is_empty()
        && let Some(endpoint) = remembered
    {
        endpoint.validate()?;
        return Ok(endpoint.clone());
    }
    let remembered_entry = remembered.map(|endpoint| match endpoint {
        Endpoint::Gateway(entry) | Endpoint::Preferred { gateway: entry, .. } => entry.clone(),
        Endpoint::Lan { node_id, .. } => Entry {
            origin: String::new(),
            node_id: node_id.clone(),
        },
        Endpoint::Local(grant) => grant.entry().clone(),
    });
    let entry = connection_entry(origin, node_id, remembered_entry.as_ref().or(hint), false)?;
    let direct = if !addresses.trim().is_empty() {
        addresses
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .map(str::to_owned)
            .collect::<Vec<_>>()
    } else {
        match remembered.filter(|endpoint| endpoint.node_id() == entry.node_id) {
            Some(Endpoint::Lan { addresses, .. } | Endpoint::Preferred { addresses, .. }) => {
                addresses.clone()
            }
            _ => vec![],
        }
    };
    let endpoint = if transport == "gateway" {
        Endpoint::Gateway(entry)
    } else if transport == "lan" || entry.origin.is_empty() {
        Endpoint::Lan {
            node_id: entry.node_id,
            addresses: direct,
        }
    } else if direct.is_empty() {
        Endpoint::Gateway(entry)
    } else {
        Endpoint::Preferred {
            gateway: entry,
            addresses: direct,
        }
    };
    endpoint.validate()?;
    Ok(endpoint)
}

const TRANSFER_MAX_AMOUNT: &str =
    "115792089237316195423570985008687907853269984665640564039457584007913129639935";
pub fn transfer_amount_valid(value: &str) -> bool {
    !value.is_empty()
        && value.bytes().all(|byte| byte.is_ascii_digit())
        && (value == "0" || !value.starts_with('0'))
        && (value.len() < TRANSFER_MAX_AMOUNT.len()
            || value.len() == TRANSFER_MAX_AMOUNT.len() && value <= TRANSFER_MAX_AMOUNT)
}
pub fn transfer_amount_at_most(value: &str, cap: &str) -> bool {
    transfer_amount_valid(value)
        && transfer_amount_valid(cap)
        && (value.len() < cap.len() || value.len() == cap.len() && value <= cap)
}
pub fn transfer_sum<'a>(amounts: impl IntoIterator<Item = &'a str>) -> anyhow::Result<String> {
    let mut total = "0".to_owned();
    for amount in amounts {
        anyhow::ensure!(
            transfer_amount_valid(amount),
            "金额须为256位范围内的最小单位非负整数"
        );
        let mut a = total.bytes().rev();
        let mut b = amount.bytes().rev();
        let mut carry = 0;
        let mut digits = vec![];
        loop {
            let left = a.next();
            let right = b.next();
            if left.is_none() && right.is_none() && carry == 0 {
                break;
            }
            let sum =
                left.map_or(0, |byte| byte - b'0') + right.map_or(0, |byte| byte - b'0') + carry;
            digits.push(b'0' + sum % 10);
            carry = sum / 10;
        }
        digits.reverse();
        total = digits.into_iter().map(char::from).collect();
        anyhow::ensure!(transfer_amount_valid(&total), "金额合计超出256位范围");
    }
    Ok(total)
}
fn transfer_peer_valid(peer: &str) -> bool {
    peer.len() == 64
        && peer
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}
fn transfer_address_valid(address: &str) -> bool {
    address.len() == 42
        && address.starts_with("0x")
        && address[2..].bytes().all(|byte| byte.is_ascii_hexdigit())
}
pub fn parse_transfer_budgets(value: serde_json::Value) -> anyhow::Result<Vec<TransferBudget>> {
    let budgets: Vec<TransferBudget> = decode_list(&value, "budgets")?;
    anyhow::ensure!(budgets.len() <= 128, "预算列表超过设备上限");
    let mut ids = std::collections::HashSet::new();
    for budget in &budgets {
        validate_transfer_budget(budget)?;
        anyhow::ensure!(ids.insert(&budget.budget_id), "预算ID重复，已暂停预算操作");
    }
    Ok(budgets)
}
pub fn validate_transfer_budget(budget: &TransferBudget) -> anyhow::Result<()> {
    anyhow::ensure!(
        transfer_peer_valid(&budget.budget_id)
            && transfer_peer_valid(&budget.provider_peer)
            && [&budget.payer, &budget.payee, &budget.asset]
                .iter()
                .all(|value| transfer_address_valid(value)),
        "预算固定身份无效"
    );
    anyhow::ensure!(
        !budget.payer.eq_ignore_ascii_case(&budget.payee)
            && ["open", "closing", "closed"].contains(&budget.state.as_str()),
        "预算角色或状态无效"
    );
    anyhow::ensure!(
        [
            &budget.deposit,
            &budget.free,
            &budget.locked,
            &budget.earned,
            &budget.withdrawn,
            &budget.refunded,
            &budget.gas_cap
        ]
        .iter()
        .all(|value| transfer_amount_valid(value)),
        "预算金额无效"
    );
    anyhow::ensure!(
        transfer_sum([
            budget.free.as_str(),
            budget.locked.as_str(),
            budget.earned.as_str(),
            budget.refunded.as_str()
        ])? == budget.deposit
            && transfer_amount_at_most(&budget.withdrawn, &budget.earned),
        "预算账目不守恒"
    );
    let mut windows = std::collections::HashSet::new();
    anyhow::ensure!(
        budget.ready_windows.len() <= 256,
        "预算就绪片段超过设备上限"
    );
    for window in &budget.ready_windows {
        anyhow::ensure!(
            transfer_peer_valid(&window.order_id)
                && window.index < 2
                && windows.insert((&window.order_id, window.index)),
            "预算就绪片段重复或无效"
        );
    }
    Ok(())
}
pub fn transfer_confirmable(order: &TransferOrder, now: u64) -> bool {
    order.role == "sender"
        && order.phase == "prepared"
        && (order.mode == "charity"
            || order.mode == "paid"
                && order.acceptance == "recoverable"
                && order.payment.budget_id.is_some()
                && order.payment.state == "prepared"
                && order
                    .deadlines
                    .as_ref()
                    .is_some_and(|deadlines| now <= deadlines.quote_expires_at))
}
pub fn transfer_acceptable(order: &TransferOrder) -> bool {
    order.role == "receiver"
        && !can_read_transfer(order)
        && !["refunded", "failed"].contains(&order.phase.as_str())
        && (order.mode == "charity"
            || order.mode == "paid"
                && order.acceptance == "recoverable"
                && order.payment.budget_id.is_some())
}
pub fn validate_transfer_pair(
    orders: &[TransferOrder; 2],
    accept: bool,
    now: u64,
) -> anyhow::Result<()> {
    for order in orders {
        validate_transfer_order(order)?;
        let eligible = if accept {
            transfer_acceptable(order)
        } else {
            transfer_confirmable(order, now)
        };
        anyhow::ensure!(
            order.mode == "paid"
                && order.acceptance == "recoverable"
                && order.windows.len() == 1
                && eligible,
            "批次需要两笔各含一个片段、当前可操作的受保护订单"
        );
    }
    let [a, b] = orders;
    anyhow::ensure!(
        a.order_id != b.order_id
            && a.intent_id != b.intent_id
            && a.payment.budget_id.is_some()
            && a.payment.budget_id == b.payment.budget_id
            && (if accept {
                a.receiver_peer == b.receiver_peer
            } else {
                a.source_peer == b.source_peer
            })
            && a.provider_peer == b.provider_peer
            && a.payment.payer == b.payment.payer
            && a.payment.provider_wallet == b.payment.provider_wallet
            && a.payment.asset == b.payment.asset
            && a.payment.chain_id == b.payment.chain_id
            && a.payment.gas_cap == b.payment.gas_cap,
        "两笔订单的预算、设备或固定付款条款不同"
    );
    Ok(())
}
pub fn transfer_batches(
    orders: &[TransferOrder],
    accept: bool,
    now: u64,
) -> Vec<[TransferOrder; 2]> {
    let mut grouped = BTreeMap::<String, Vec<TransferOrder>>::new();
    for order in orders {
        let eligible = if accept {
            transfer_acceptable(order)
        } else {
            transfer_confirmable(order, now)
        };
        if order.mode == "paid"
            && eligible
            && let Some(id) = &order.payment.budget_id
        {
            grouped.entry(id.clone()).or_default().push(order.clone());
        }
    }
    grouped
        .into_values()
        .filter_map(|orders| {
            let pair: [TransferOrder; 2] = orders.try_into().ok()?;
            validate_transfer_pair(&pair, accept, now).ok()?;
            Some(pair)
        })
        .collect()
}
pub fn transfer_payment_label(state: &str) -> &'static str {
    match state {
        "free" => "免费",
        "prepared" => "预算待分配",
        "confirmed" => "预算已分配",
        "earned" => "服务费已赚取",
        "withdrawn" => "预算费用已提取",
        "refunded" => "款已退回",
        "unknown" => "付款结果未知",
        "failed" => "付款失败",
        _ => "付款状态待核对",
    }
}
pub fn transfer_settlement_uncertain(order: &TransferOrder) -> bool {
    ["unknown", "failed"].contains(&order.payment.state.as_str())
        || ["reconciling", "failed"].contains(&order.phase.as_str())
}
pub fn transfer_publication_label(order: &TransferOrder) -> &'static str {
    if order.mode == "charity" {
        return "免费传输，不发生付款或预算结算";
    }
    if transfer_settlement_uncertain(order) {
        return "批次状态待确认，请检查原交易";
    }
    let Some(publication) = order
        .payment
        .settlement
        .as_ref()
        .and_then(|settlement| settlement.publication.as_ref())
    else {
        return if order.payment.earned != "0" {
            "本单费用已赚取"
        } else if order.payment.refunded != "0" {
            "本单费用已退款"
        } else {
            "等待中转设备结算已就绪批次"
        };
    };
    match publication.state {
        TransferPublicationState::PrivateReady => "恢复材料仍私密",
        TransferPublicationState::DisclosedOutstanding => "恢复材料可能已公开，正在确认收入",
        TransferPublicationState::FinalEarned => "本批约定费用已赚取",
        TransferPublicationState::FinalSettled => "本批款项已赚取或退款，恢复材料可能已公开",
        TransferPublicationState::ExpiredUnrevealed => "已过期，尚未公开恢复材料",
    }
}
impl TransferBudget {
    pub fn batch_binding(&self) -> vibeface_client::DeliveryBudgetBatch {
        vibeface_client::DeliveryBudgetBatch {
            budget_id: self.budget_id.clone(),
            payer: self.payer.clone(),
            payee: self.payee.clone(),
            provider_peer: self.provider_peer.clone(),
            asset: self.asset.clone(),
            ready_windows: self
                .ready_windows
                .iter()
                .map(|window| vibeface_client::DeliveryReadyWindow {
                    order_id: window.order_id.clone(),
                    index: window.index,
                })
                .collect(),
        }
    }
}

pub fn transfer_flush_review(
    budget: &TransferBudget,
    orders: &[TransferOrder],
) -> anyhow::Result<TransferFlush> {
    validate_transfer_budget(budget)?;
    vibeface_client::delivery_flush_batch_id(&budget.batch_binding())?;
    let mut selected = vec![];
    for window in &budget.ready_windows {
        let order = orders
            .iter()
            .find(|order| order.order_id == window.order_id)
            .ok_or_else(|| anyhow::anyhow!("原就绪片段的订单尚未读回"))?;
        validate_transfer_order(order)?;
        anyhow::ensure!(
            order.role == "provider"
                && order.mode == "paid"
                && order.acceptance == "recoverable"
                && order.provider_peer == budget.provider_peer
                && order.payment.budget_id.as_deref() == Some(budget.budget_id.as_str())
                && order
                    .payment
                    .payer
                    .as_deref()
                    .is_some_and(|payer| payer.eq_ignore_ascii_case(&budget.payer))
                && order
                    .payment
                    .provider_wallet
                    .as_deref()
                    .is_some_and(|payee| payee.eq_ignore_ascii_case(&budget.payee))
                && order
                    .payment
                    .asset
                    .as_deref()
                    .is_some_and(|asset| asset.eq_ignore_ascii_case(&budget.asset))
                && order
                    .windows
                    .iter()
                    .any(|item| item.index == u64::from(window.index)),
            "原就绪片段与预算或订单固定条款不同"
        );
        if !selected
            .iter()
            .any(|item: &TransferOrder| item.order_id == order.order_id)
        {
            selected.push(order.clone());
        }
    }
    Ok(TransferFlush {
        budget: budget.clone(),
        orders: selected,
    })
}
