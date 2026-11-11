use gpui_kit::{
    component::input::{Input, InputState, Textarea, TextareaState},
    *,
};
use std::collections::BTreeMap;
#[derive(Clone, Debug)]
pub enum Editor {
    Connect,
    ConnectLocal,
    CreateProfile,
    CreateNetwork,
    Join,
    CreateBot,
    EditBot,
    BotModel(vibeface_client::Provider),
    CloseSettings(vibeface_client::SettingsReview),
    CreateTask,
    EditTask(String, u64),
    CreateChannel,
    ChannelPost(String),
    Member(String),
    EditChannel(String, u64, u64),
    Memory(Option<crate::ui::Memory>),
    Schedule,
    EditSchedule(crate::ui::Schedule),
    AssignTask(String, u64),
    QueuedEdit(vibeface_protocol::workspace::QueuedInput),
    Provider,
    DecisionToken,
    NodeSettings,
    Triage,
    Avatar,
    Logs,
    Relay,
    Transfer,
    Answer(String, String),
    Recovery,
}
pub struct Field {
    pub key: &'static str,
    pub label: &'static str,
    pub state: Entity<InputState>,
    pub area: Option<Entity<TextareaState>>,
}
pub struct Form {
    pub submission_id: String,
    pub generation: u64,
    pub owner_node_id: Option<String>,
    pub editor: Editor,
    pub fields: Vec<Field>,
    delivery_budget_id: Option<String>,
}
impl Form {
    pub fn new(
        editor: Editor,
        values: BTreeMap<&str, String>,
        generation: u64,
        window: &mut Window,
        cx: &mut App,
    ) -> Self {
        let specs: &[(&str, &str, bool)] = match &editor {
            Editor::CreateProfile => &[("label", "个人网络档案名称", false)],
            Editor::ConnectLocal => &[
                ("unlock_mode", "设备解锁方式 vault 或 manual", false),
                (
                    "unlock_password",
                    "manual 方式的设备解锁口令，每次启动输入",
                    true,
                ),
            ],
            Editor::Connect => &[
                ("unlock_mode", "设备解锁方式 vault 或 manual", false),
                (
                    "unlock_password",
                    "manual 方式的设备解锁口令，每次启动输入",
                    true,
                ),
                (
                    "transport",
                    "接入方式 auto、gateway 或 lan，空白保留已存入口",
                    false,
                ),
                (
                    "addresses",
                    "数字 LAN 地址 IP:端口，每行一个；auto 优先直连后回 Gateway",
                    false,
                ),
                ("origin", "Gateway地址，空白使用档案保存入口", false),
                ("node_id", "目标Node公钥，空白使用档案保存目标", false),
            ],
            Editor::Join => &[
                ("unlock_mode", "设备解锁方式 vault 或 manual", false),
                (
                    "unlock_password",
                    "manual 方式的设备解锁口令，每次启动输入",
                    true,
                ),
                ("origin", "Gateway 地址，有数字直连邀请时可留空", false),
                ("ticket", "邀请链接", true),
                ("name", "设备名称", false),
            ],
            Editor::CreateNetwork => &[
                ("unlock_mode", "设备解锁方式 vault 或 manual", false),
                (
                    "unlock_password",
                    "manual 方式的设备解锁口令，每次启动输入",
                    true,
                ),
                ("name", "本设备名称", false),
                ("node_name", "Node 名称", false),
                ("password", "新恢复文件口令", true),
                ("new_file", "恢复文件保存路径", false),
            ],
            Editor::Recovery => &[
                ("unlock_mode", "设备解锁方式 vault 或 manual", false),
                (
                    "unlock_password",
                    "manual 方式的设备解锁口令，每次启动输入",
                    true,
                ),
                ("origin", "Gateway 地址", false),
                ("node_id", "目标 Node 公钥", false),
                ("name", "设备名称", false),
                ("password", "恢复文件口令", true),
                ("old_file", "原恢复文件路径", false),
                ("new_file", "新恢复文件保存路径", false),
            ],
            Editor::CreateBot | Editor::EditBot => &[
                ("name", "Bot 名称", false),
                ("instructions", "指令", false),
                ("target_node", "项目设备 ID", false),
                ("cwd", "项目目录", false),
            ],
            Editor::CloseSettings(_) => &[(
                "note",
                "说明你如何核对原设置；关闭不会确认服务器已执行",
                false,
            )],
            Editor::BotModel(_) => &[
                ("model", "模型，空白表示设备默认", false),
                ("effort", "推理强度，空白表示由Provider控制", false),
            ],
            Editor::CreateTask | Editor::EditTask(..) => &[
                ("title", "任务标题", false),
                ("owner_bot", "负责的 Bot ID", false),
                ("request", "任务要求", false),
                ("expected_result", "预期结果", false),
                ("dependencies", "依赖任务 ID，逗号分隔", false),
            ],
            Editor::Member(_) => &[("name", "设备名称", false)],
            Editor::ChannelPost(_) => &[
                ("text", "频道消息", false),
                ("recipient_bot", "@ Bot ID，可留空", false),
                ("task_id", "任务 ID，可留空", false),
                ("reply_to", "回复消息 ID，可留空", false),
            ],
            Editor::CreateChannel | Editor::EditChannel(..) => &[
                ("title", "频道名称", false),
                ("purpose", "频道用途", false),
                ("members", "成员 Bot ID，逗号分隔", false),
                ("coordinator_bot", "协调 Bot ID", false),
                ("coordinator_node", "协调 Node ID", false),
            ],
            Editor::Memory(_) => &[("text", "记忆内容", false)],
            Editor::AssignTask(..) => &[
                ("bot_id", "执行 Bot ID", false),
                ("execution_node", "执行 Node ID", false),
                ("resource_roots", "资源根目录，每行一个", false),
                ("automatic", "自动接手，true 或 false", false),
            ],
            Editor::QueuedEdit(_) => &[("text", "修改待发输入", false)],
            Editor::Schedule | Editor::EditSchedule(_) => &[
                ("name", "计划名称", false),
                ("prompt", "计划指令", false),
                ("first_run_at_ms", "首次执行时间，Unix 毫秒", false),
                ("every_minutes", "间隔分钟，空白表示单次", false),
            ],
            Editor::Provider => &[
                ("device_id", "设备 ID", false),
                ("provider", "Provider", false),
                ("bin", "程序路径", false),
                ("model", "模型", false),
                ("bot", "允许 Bot，true 或 false", false),
                ("execution", "允许执行，true 或 false", false),
                ("connect_local", "连接本机 App Server，true 或 false", false),
                ("app_server_mode", "App Server 模式 spawn 或 system", false),
                ("socket", "App Server 套接字路径", false),
                ("env", "环境变量，每行 NAME=value", true),
            ],
            Editor::Logs => &[
                ("search", "搜索日志", false),
                ("level", "级别 info/warn/error，可留空", false),
                ("source", "来源，逗号分隔，可留空", false),
                ("since_ms", "开始时间 Unix 毫秒，可留空", false),
                ("until_ms", "结束时间 Unix 毫秒，可留空", false),
                ("conversation_id", "对话 ID，可留空", false),
                ("work_id", "工作 ID，可留空", false),
            ],
            Editor::Avatar => &[
                ("outline", "轮廓 round/squircle/drop/bean/cloud", false),
                ("eyes", "眼睛 dot/pill/oval/wide", false),
                ("hue", "色相 0 到 359", false),
            ],
            Editor::DecisionToken => &[("label", "授权码名称", false)],
            Editor::NodeSettings => &[
                ("roots", "允许的项目根目录，每行一个", false),
                ("provider", "协调 Provider，空白表示关闭", false),
                ("workspace", "协调工作目录", false),
                ("model", "协调模型", false),
                ("reasoning_effort", "推理强度", false),
                (
                    "backup_automatic",
                    "允许自动切换备用模型，true或false",
                    false,
                ),
                (
                    "backup_candidates",
                    "备用模型，每行 Provider|模型|推理强度",
                    false,
                ),
            ],
            Editor::Triage => &[
                ("default_outlet", "默认处理 record 或 both", false),
                (
                    "projects",
                    "项目规则，每行 prefix=record 或 prefix=both",
                    false,
                ),
                ("webhook_url", "Webhook 地址，空白表示关闭", false),
                (
                    "scan_codex_sessions",
                    "扫描 Codex 会话，true 或 false",
                    false,
                ),
                ("codex_sessions", "Codex 会话目录，可留空", false),
                ("webhook_secret", "Webhook 密钥，空白表示保留", true),
                (
                    "clear_webhook_secret",
                    "清除现有Webhook密钥，true或false",
                    false,
                ),
            ],
            Editor::Relay => &[
                ("device_id", "设备 ID", false),
                ("url", "中继地址", false),
                ("volunteer", "提供中继服务，true 或 false", false),
                ("listen", "监听地址，可留空", false),
                ("rate", "速率上限，可留空", false),
                ("max_clients", "客户端上限，可留空", false),
                ("max_clients_per_ip", "每个 IP 上限，可留空", false),
            ],
            Editor::Transfer => &[
                ("file", "文件路径", false),
                ("receiver_peer", "接收设备 ID", false),
                ("provider_peer", "传输服务设备 ID", false),
                ("mode", "传输方式 charity 或 paid", false),
                ("max_service_amount", "最高服务费", false),
                ("max_gas_amount", "最高网络费", false),
            ],
            Editor::Answer(..) => &[("text", "回复内容", false)],
        };
        let fields = specs
            .iter()
            .map(|&(key, label, secret)| {
                let state = cx.new(|cx| {
                    InputState::new(window, cx)
                        .placeholder(label)
                        .masked(secret)
                });
                if let Some(value) = values.get(key) {
                    state.update(cx, |s, cx| s.set_value(value.clone(), window, cx));
                }
                let area = if !secret
                    && [
                        "text",
                        "instructions",
                        "request",
                        "expected_result",
                        "prompt",
                        "roots",
                        "projects",
                        "addresses",
                        "backup_candidates",
                        "note",
                    ]
                    .contains(&key)
                {
                    let area = cx.new(|cx| TextareaState::new(window, cx).placeholder(label));
                    if let Some(value) = values.get(key) {
                        area.update(cx, |s, cx| s.set_value(value.clone(), window, cx));
                    }
                    Some(area)
                } else {
                    None
                };
                Field {
                    key,
                    label,
                    state,
                    area,
                }
            })
            .collect();
        Self {
            submission_id: uuid::Uuid::new_v4().to_string(),
            generation,
            owner_node_id: values.get("_owner_node_id").cloned(),
            delivery_budget_id: if matches!(&editor, Editor::Transfer) {
                values.get("budget_id").cloned()
            } else {
                None
            },
            editor,
            fields,
        }
    }
    pub fn values(&self, cx: &App) -> BTreeMap<String, String> {
        let mut values: BTreeMap<_, _> = self
            .fields
            .iter()
            .map(|f| {
                (
                    f.key.to_owned(),
                    f.area
                        .as_ref()
                        .map(|area| area.read(cx).value().to_string())
                        .unwrap_or_else(|| f.state.read(cx).value().to_string()),
                )
            })
            .collect();
        values.insert("_submission_id".into(), self.submission_id.clone());
        if let Some(node) = &self.owner_node_id {
            values.insert("_owner_node_id".into(), node.clone());
        }
        if let Some(budget) = &self.delivery_budget_id {
            values.insert("budget_id".into(), budget.clone());
        }
        values
    }
    pub fn render(&self) -> AnyElement {
        div()
            .flex()
            .flex_col()
            .gap_4()
            .children(self.delivery_budget_id.as_ref().map(|id| {
                div().child(format!(
                    "固定预付预算 {id}。受保护付费仅使用可恢复验收；更换预算需从预算列表重新准备。"
                ))
            }))
            .children(self.fields.iter().map(|f| {
                let input = if let Some(area) = &f.area {
                    Textarea::new(area)
                        .h(px(100.))
                        .aria_label(f.label)
                        .into_any_element()
                } else {
                    Input::new(&f.state).aria_label(f.label).into_any_element()
                };
                div()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .child(div().text_sm().child(f.label))
                    .child(input)
            }))
            .into_any_element()
    }
}

impl Editor {
    pub fn needs_node_operator(&self) -> bool {
        matches!(
            self,
            Self::Provider
                | Self::DecisionToken
                | Self::NodeSettings
                | Self::Triage
                | Self::Relay
                | Self::Logs
        )
    }
}
