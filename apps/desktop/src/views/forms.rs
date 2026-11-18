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
    pub control: FieldControl,
}
pub enum FieldControl {
    SingleLine(Entity<InputState>),
    MultiLine(Entity<TextareaState>),
}
impl FieldControl {
    pub fn set_value(&self, value: impl Into<SharedString>, window: &mut Window, cx: &mut App) {
        let value: SharedString = value.into();
        match self {
            Self::SingleLine(state) => {
                state.update(cx, |state, cx| state.set_value(value, window, cx));
            }
            Self::MultiLine(state) => {
                state.update(cx, |state, cx| state.set_value(value, window, cx));
            }
        }
    }
}
#[derive(Clone, Copy)]
enum FieldKind {
    SingleLine,
    Password,
    MultiLine,
}
struct TaskMetadata {
    title: String,
    owner_bot: String,
}
pub struct Form {
    pub submission_id: String,
    pub generation: u64,
    pub owner_node_id: Option<String>,
    pub editor: Editor,
    pub fields: Vec<Field>,
    task_metadata: Option<TaskMetadata>,
    local_node_id: Option<String>,
    local_data_dir: Option<String>,
    delivery_budget_id: Option<String>,
    delivery_restart_order_id: Option<String>,
    delivery_restart_intent_id: Option<String>,
    delivery_restart_review: Option<String>,
}
impl Form {
    pub fn new(
        editor: Editor,
        values: BTreeMap<&str, String>,
        generation: u64,
        window: &mut Window,
        cx: &mut App,
    ) -> Self {
        use FieldKind::{MultiLine, Password, SingleLine};
        let specs: &[(&str, &str, FieldKind)] = match &editor {
            Editor::CreateProfile => &[("label", "个人网络档案名称", SingleLine)],
            Editor::ConnectLocal => &[
                ("unlock_mode", "设备解锁方式 vault 或 manual", SingleLine),
                (
                    "unlock_password",
                    "manual 方式的设备解锁口令，每次启动输入",
                    Password,
                ),
            ],
            Editor::Connect => &[
                ("unlock_mode", "设备解锁方式 vault 或 manual", SingleLine),
                (
                    "unlock_password",
                    "manual 方式的设备解锁口令，每次启动输入",
                    Password,
                ),
                (
                    "transport",
                    "接入方式 auto、gateway 或 lan，空白保留已存入口",
                    SingleLine,
                ),
                (
                    "addresses",
                    "数字 LAN 地址 IP:端口，每行一个；auto 优先直连后回 Gateway",
                    MultiLine,
                ),
                ("origin", "Gateway地址，空白使用档案保存入口", SingleLine),
                ("node_id", "目标Node公钥，空白使用档案保存目标", SingleLine),
            ],
            Editor::Join => &[
                ("unlock_mode", "设备解锁方式 vault 或 manual", SingleLine),
                (
                    "unlock_password",
                    "manual 方式的设备解锁口令，每次启动输入",
                    Password,
                ),
                ("origin", "Gateway 地址，有数字直连邀请时可留空", SingleLine),
                ("ticket", "邀请链接", Password),
                ("name", "设备名称", SingleLine),
            ],
            Editor::CreateNetwork => &[
                ("unlock_mode", "设备解锁方式 vault 或 manual", SingleLine),
                (
                    "unlock_password",
                    "manual 方式的设备解锁口令，每次启动输入",
                    Password,
                ),
                ("name", "本设备名称", SingleLine),
                ("node_name", "Node 名称", SingleLine),
                ("password", "新恢复文件口令", Password),
                ("new_file", "恢复文件保存路径", SingleLine),
            ],
            Editor::Recovery => &[
                ("unlock_mode", "设备解锁方式 vault 或 manual", SingleLine),
                (
                    "unlock_password",
                    "manual 方式的设备解锁口令，每次启动输入",
                    Password,
                ),
                ("origin", "Gateway 地址", SingleLine),
                ("node_id", "目标 Node 公钥", SingleLine),
                ("name", "设备名称", SingleLine),
                ("password", "恢复文件口令", Password),
                ("old_file", "原恢复文件路径", SingleLine),
                ("new_file", "新恢复文件保存路径", SingleLine),
            ],
            Editor::CreateBot | Editor::EditBot => &[
                ("name", "Bot 名称", SingleLine),
                ("instructions", "指令", MultiLine),
                ("target_node", "项目设备 ID", SingleLine),
                ("cwd", "项目目录", SingleLine),
            ],
            Editor::CloseSettings(_) => &[(
                "note",
                "说明你如何核对原设置；关闭不会确认服务器已执行",
                MultiLine,
            )],
            Editor::BotModel(_) => &[
                ("model", "模型，空白表示设备默认", SingleLine),
                ("effort", "推理强度，空白表示由Provider控制", SingleLine),
            ],
            Editor::CreateTask => &[
                ("title", "任务标题", SingleLine),
                ("owner_bot", "负责的 Bot ID", SingleLine),
                ("request", "任务要求", MultiLine),
                ("expected_result", "预期结果", MultiLine),
                ("dependencies", "依赖任务 ID，逗号分隔", SingleLine),
            ],
            Editor::EditTask(..) => &[
                ("request", "任务要求", MultiLine),
                ("expected_result", "预期结果", MultiLine),
                ("dependencies", "依赖任务 ID，逗号分隔", SingleLine),
            ],
            Editor::Member(_) => &[("name", "设备名称", SingleLine)],
            Editor::ChannelPost(_) => &[
                ("text", "频道消息", MultiLine),
                ("recipient_bot", "@ Bot ID，可留空", SingleLine),
                ("task_id", "任务 ID，可留空", SingleLine),
                ("reply_to", "回复消息 ID，可留空", SingleLine),
            ],
            Editor::CreateChannel | Editor::EditChannel(..) => &[
                ("title", "频道名称", SingleLine),
                ("purpose", "频道用途", SingleLine),
                ("members", "成员 Bot ID，逗号分隔", SingleLine),
                ("coordinator_bot", "协调 Bot ID", SingleLine),
                ("coordinator_node", "协调 Node ID", SingleLine),
            ],
            Editor::Memory(_) => &[("text", "记忆内容", MultiLine)],
            Editor::AssignTask(..) => &[
                ("bot_id", "执行 Bot ID", SingleLine),
                ("execution_node", "执行 Node ID", SingleLine),
                ("resource_roots", "资源根目录，每行一个", SingleLine),
                ("automatic", "自动接手，true 或 false", SingleLine),
            ],
            Editor::QueuedEdit(_) => &[("text", "修改待发输入", MultiLine)],
            Editor::Schedule | Editor::EditSchedule(_) => &[
                ("name", "计划名称", SingleLine),
                ("prompt", "计划指令", MultiLine),
                ("first_run_at_ms", "首次执行时间，Unix 毫秒", SingleLine),
                ("every_minutes", "间隔分钟，空白表示单次", SingleLine),
            ],
            Editor::Provider => &[
                ("device_id", "设备 ID", SingleLine),
                ("provider", "Provider", SingleLine),
                ("bin", "程序路径", SingleLine),
                ("model", "模型", SingleLine),
                ("bot", "允许 Bot，true 或 false", SingleLine),
                ("execution", "允许执行，true 或 false", SingleLine),
                (
                    "connect_local",
                    "连接本机 App Server，true 或 false",
                    SingleLine,
                ),
                (
                    "app_server_mode",
                    "App Server 模式 spawn 或 system",
                    SingleLine,
                ),
                ("socket", "App Server 套接字路径", SingleLine),
                ("env", "环境变量，每行 NAME=value", Password),
            ],
            Editor::Logs => &[
                ("search", "搜索日志", SingleLine),
                ("level", "级别 info/warn/error，可留空", SingleLine),
                ("source", "来源，逗号分隔，可留空", SingleLine),
                ("since_ms", "开始时间 Unix 毫秒，可留空", SingleLine),
                ("until_ms", "结束时间 Unix 毫秒，可留空", SingleLine),
                ("conversation_id", "对话 ID，可留空", SingleLine),
                ("work_id", "工作 ID，可留空", SingleLine),
            ],
            Editor::Avatar => &[
                ("outline", "轮廓 round/squircle/drop/bean/cloud", SingleLine),
                ("eyes", "眼睛 dot/pill/oval/wide", SingleLine),
                ("hue", "色相 0 到 359", SingleLine),
            ],
            Editor::DecisionToken => &[("label", "授权码名称", SingleLine)],
            Editor::NodeSettings => &[
                ("roots", "允许的项目根目录，每行一个", MultiLine),
                ("provider", "协调 Provider，空白表示关闭", SingleLine),
                ("workspace", "协调工作目录", SingleLine),
                ("model", "协调模型", SingleLine),
                ("reasoning_effort", "推理强度", SingleLine),
                (
                    "backup_automatic",
                    "允许自动切换备用模型，true或false",
                    SingleLine,
                ),
                (
                    "backup_candidates",
                    "备用模型，每行 Provider|模型|推理强度",
                    MultiLine,
                ),
            ],
            Editor::Triage => &[
                ("default_outlet", "默认处理 record 或 both", SingleLine),
                (
                    "projects",
                    "项目规则，每行 prefix=record 或 prefix=both",
                    MultiLine,
                ),
                ("webhook_url", "Webhook 地址，空白表示关闭", SingleLine),
                (
                    "scan_codex_sessions",
                    "扫描 Codex 会话，true 或 false",
                    SingleLine,
                ),
                ("codex_sessions", "Codex 会话目录，可留空", SingleLine),
                ("webhook_secret", "Webhook 密钥，空白表示保留", Password),
                (
                    "clear_webhook_secret",
                    "清除现有Webhook密钥，true或false",
                    SingleLine,
                ),
            ],
            Editor::Relay => &[
                ("device_id", "设备 ID", SingleLine),
                ("url", "中继地址", SingleLine),
                ("volunteer", "提供中继服务，true 或 false", SingleLine),
                ("listen", "监听地址，可留空", SingleLine),
                ("rate", "速率上限，可留空", SingleLine),
                ("max_clients", "客户端上限，可留空", SingleLine),
                ("max_clients_per_ip", "每个 IP 上限，可留空", SingleLine),
            ],
            Editor::Transfer => &[
                ("file", "文件路径", SingleLine),
                ("receiver_peer", "接收设备 ID", SingleLine),
                ("provider_peer", "传输服务设备 ID", SingleLine),
                ("mode", "传输方式 charity 或 paid", SingleLine),
                ("max_service_amount", "最高服务费", SingleLine),
                ("max_gas_amount", "最高网络费", SingleLine),
            ],
            Editor::Answer(..) => &[("text", "回复内容", MultiLine)],
        };
        let fields = specs
            .iter()
            .map(|&(key, label, kind)| {
                let control = match kind {
                    FieldKind::SingleLine | FieldKind::Password => {
                        FieldControl::SingleLine(cx.new(|cx| {
                            InputState::new(window, cx)
                                .placeholder(label)
                                .masked(matches!(kind, FieldKind::Password))
                        }))
                    }
                    FieldKind::MultiLine => FieldControl::MultiLine(
                        cx.new(|cx| TextareaState::new(window, cx).placeholder(label)),
                    ),
                };
                if let Some(value) = values.get(key) {
                    control.set_value(value.clone(), window, cx);
                }
                Field {
                    key,
                    label,
                    control,
                }
            })
            .collect();
        Self {
            submission_id: uuid::Uuid::new_v4().to_string(),
            generation,
            owner_node_id: values.get("_owner_node_id").cloned(),
            local_node_id: values.get("_local_node_id").cloned(),
            local_data_dir: values.get("_local_data_dir").cloned(),
            task_metadata: matches!(&editor, Editor::EditTask(..)).then(|| TaskMetadata {
                title: values.get("title").cloned().unwrap_or_default(),
                owner_bot: values.get("owner_bot").cloned().unwrap_or_default(),
            }),
            delivery_budget_id: if matches!(&editor, Editor::Transfer) {
                values.get("budget_id").cloned()
            } else {
                None
            },
            delivery_restart_order_id: if matches!(&editor, Editor::Transfer) {
                values.get("_restart_order_id").cloned()
            } else {
                None
            },
            delivery_restart_intent_id: if matches!(&editor, Editor::Transfer) {
                values.get("_restart_intent_id").cloned()
            } else {
                None
            },
            delivery_restart_review: if matches!(&editor, Editor::Transfer) {
                values.get("_restart_review").cloned()
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
                    match &f.control {
                        FieldControl::SingleLine(state) => state.read(cx).value().to_string(),
                        FieldControl::MultiLine(state) => state.read(cx).value().to_string(),
                    },
                )
            })
            .collect();
        values.insert("_submission_id".into(), self.submission_id.clone());
        if let Some(node) = &self.owner_node_id {
            values.insert("_owner_node_id".into(), node.clone());
        }
        if let Some(node) = &self.local_node_id {
            values.insert("_local_node_id".into(), node.clone());
        }
        if let Some(path) = &self.local_data_dir {
            values.insert("_local_data_dir".into(), path.clone());
        }
        if let Some(budget) = &self.delivery_budget_id {
            values.insert("budget_id".into(), budget.clone());
        }
        if let Some(id) = &self.delivery_restart_order_id {
            values.insert("_restart_order_id".into(), id.clone());
        }
        if let Some(id) = &self.delivery_restart_intent_id {
            values.insert("_restart_intent_id".into(), id.clone());
        }
        if let Some(review) = &self.delivery_restart_review {
            values.insert("_restart_review".into(), review.clone());
        }
        values
    }
    pub fn render(&self) -> AnyElement {
        div()
            .flex()
            .flex_col()
            .gap_4()
            .children(self.task_metadata.as_ref().map(|task| {
                div()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .child(format!("任务标题（只读）：{}", task.title))
                    .child(format!("负责的 Bot ID（只读）：{}", task.owner_bot))
            }))
            .children(self.delivery_restart_order_id.as_ref().map(|id| div().child(format!("作为新订单再次发送：原订单 {id}。请重新选择同名、同内容的原文件；原条款和金额上限必须保持一致，不会自动确认或付款。"))))
            .children(self.delivery_budget_id.as_ref().map(|id| {
                div().child(format!(
                    "固定预付预算 {id}。受保护付费仅使用可恢复验收；更换预算需从预算列表重新准备。"
                ))
            }))
            .children(self.fields.iter().map(|f| {
                let input = match &f.control {
                    FieldControl::SingleLine(state) => {
                        Input::new(state).aria_label(f.label).into_any_element()
                    }
                    FieldControl::MultiLine(state) => Textarea::new(state)
                        .h(px(100.))
                        .aria_label(f.label)
                        .into_any_element(),
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
