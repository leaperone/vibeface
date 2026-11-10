use crate::{
    app::Desktop,
    ui::{Scope as ConversationScope, *},
    views::{backend::Command, history::*},
};
use gpui_kit::{
    component::{Disableable, button::Button},
    *,
};
use vibeface_protocol::history::HistorySection as S;

pub fn history_label(section: S) -> &'static str {
    match section {
        S::Messages => "消息",
        S::Works => "工作",
        S::Changes => "配置变更",
        S::Questions => "问题与回复",
        S::Sessions => "会话交接",
        S::Jobs => "本机回复执行",
        S::ScopeJobs => "本机任务执行",
        S::OwnerJobs => "拥有者回复执行",
        S::OwnerScopeJobs => "拥有者任务执行",
        S::OwnerMemoryConflicts => "拥有者记忆冲突",
        S::Checks => "进程核对",
        S::Deliveries => "工作投递",
        S::Unanswered => "未完成回复",
        S::Skills => "共享技能",
        S::Schedules => "计划",
        S::Memory => "记忆",
        S::MemoryConflicts => "记忆提交冲突",
        S::MemorySyncConflicts => "记忆同步冲突",
        S::Overwritten => "被覆盖的配置",
    }
}
fn history_key(section: S) -> String {
    serde_json::to_value(section)
        .expect("history section enum")
        .as_str()
        .expect("unit enum")
        .to_owned()
}
fn scope_label(scope: &ConversationScope) -> String {
    match scope {
        ConversationScope::Main => "Bot 主对话".into(),
        ConversationScope::WorkspaceTask { task_id } => format!("任务 {task_id}"),
        ConversationScope::Task { work_id } => format!("工作 {work_id}"),
        ConversationScope::Routine { trigger } => format!("例行任务 {trigger}"),
    }
}
fn face(value: &Option<ChangeAvatar>) -> String {
    value
        .as_ref()
        .map(|face| format!("{} / {} / 色相 {}", face.outline, face.eyes, face.hue))
        .unwrap_or_else(|| "使用默认头像".into())
}
fn skills(value: &Option<Vec<String>>) -> String {
    value
        .as_ref()
        .map(|items| {
            if items.is_empty() {
                "不启用技能".into()
            } else {
                items.join("、")
            }
        })
        .unwrap_or_else(|| "全部可用".into())
}
fn changed(before: &str, after: &str) -> String {
    format!("修改前\n{before}\n\n修改后\n{after}")
}
fn default_text(value: &str) -> &str {
    if value.is_empty() {
        "设备默认"
    } else {
        value
    }
}
pub fn change_fields(change: &BotChange) -> Vec<(String, String)> {
    let mut fields = vec![];
    match change {
        BotChange::Profile {
            name,
            instructions,
            model,
            reasoning_effort,
            skills: selection,
            avatar,
            ..
        } => {
            if let Some(edit) = name {
                fields.push(("名称".into(), changed(&edit.before, &edit.after)));
            }
            if let Some(edit) = instructions {
                fields.push(("指令".into(), changed(&edit.before, &edit.after)));
            }
            if let Some(edit) = model {
                fields.push((
                    format!("{:?} 模型", edit.provider),
                    changed(default_text(&edit.before), default_text(&edit.after)),
                ));
            }
            if let Some(edit) = reasoning_effort {
                fields.push((
                    format!("{:?} 推理强度", edit.provider),
                    changed(
                        edit.before.as_deref().unwrap_or("设备默认"),
                        edit.after.as_deref().unwrap_or("设备默认"),
                    ),
                ));
            }
            if let Some(edit) = selection {
                fields.push((
                    "技能范围".into(),
                    changed(&skills(&edit.before), &skills(&edit.after)),
                ));
            }
            if let Some(edit) = avatar {
                fields.push((
                    "头像".into(),
                    changed(&face(&edit.before), &face(&edit.after)),
                ));
            }
        }
        BotChange::Skill {
            skill_id,
            before,
            after,
            ..
        } => {
            let words = |value: &Option<SkillBody>| {
                value
                    .as_ref()
                    .map(|value| format!("{}\n{}\n\n{}", value.name, value.description, value.body))
                    .unwrap_or_else(|| "不存在".into())
            };
            fields.push((
                format!("技能 {skill_id}"),
                changed(&words(before), &words(after)),
            ));
        }
        BotChange::Schedule {
            schedule_id,
            before,
            after,
            ..
        } => {
            let words = |value: &Option<ScheduleBody>| {
                value
                    .as_ref()
                    .map(|value| {
                        format!(
                            "{} · {} · 首次 {} · 间隔分钟 {} · 深度 {}\n{}",
                            value.name,
                            if value.enabled { "启用" } else { "暂停" },
                            value.first_run_at_ms,
                            value
                                .every_minutes
                                .map(|v| v.to_string())
                                .unwrap_or_else(|| "单次".into()),
                            value.depth,
                            value.prompt
                        )
                    })
                    .unwrap_or_else(|| "不存在".into())
            };
            fields.push((
                format!("计划 {schedule_id}"),
                changed(&words(before), &words(after)),
            ));
        }
        BotChange::Memory {
            memory_id,
            before,
            after,
            origin,
            essential,
            ..
        } => {
            fields.push((
                format!("记忆 {memory_id}"),
                changed(
                    before.as_deref().unwrap_or("不存在"),
                    after.as_deref().unwrap_or("已删除"),
                ),
            ));
            fields.push((
                "来源与保护".into(),
                format!(
                    "{origin} · {}",
                    if *essential {
                        "必要记忆"
                    } else {
                        "普通记忆"
                    }
                ),
            ));
        }
    }
    fields
}
fn overwritten_words(field: &OverwrittenField) -> (String, String) {
    match field {
        OverwrittenField::Name { value } => ("名称".into(), value.clone()),
        OverwrittenField::Instructions { value } => ("指令".into(), value.clone()),
        OverwrittenField::Model { provider, value } => {
            (format!("{provider:?} 模型"), default_text(value).into())
        }
        OverwrittenField::ReasoningEffort { provider, value } => (
            format!("{provider:?} 推理强度"),
            value.clone().unwrap_or_else(|| "设备默认".into()),
        ),
        OverwrittenField::Skills { value } => ("技能范围".into(), skills(value)),
        OverwrittenField::Avatar { value } => ("头像".into(), face(value)),
    }
}

pub struct HistoryList {
    pub list: ListState,
    scope: String,
    keys: Vec<String>,
}
impl HistoryList {
    pub fn new() -> Self {
        Self {
            list: ListState::new(0, ListAlignment::Top, px(500.)),
            scope: String::new(),
            keys: vec![],
        }
    }
    pub fn replace(&mut self, scope: String, keys: Vec<String>) {
        let anchor = self.list.logical_scroll_top();
        let same = self.scope == scope;
        let indices = keys
            .iter()
            .enumerate()
            .map(|(index, key)| (key.as_str(), index))
            .collect::<std::collections::HashMap<_, _>>();
        let top = if same {
            self.keys
                .iter()
                .skip(anchor.item_ix)
                .chain(self.keys.iter().take(anchor.item_ix).rev())
                .find_map(|key| indices.get(key.as_str()).copied())
                .unwrap_or(0)
        } else {
            0
        };
        self.list.splice(0..self.keys.len(), keys.len());
        self.list.scroll_to(ListOffset {
            item_ix: top,
            offset_in_item: if same { anchor.offset_in_item } else { px(0.) },
        });
        drop(indices);
        self.scope = scope;
        self.keys = keys;
    }
}
fn conflict_key(conflict: &MemoryConflict) -> String {
    if conflict.history_id.is_empty() {
        format!(
            "{}:{}:{}:{}",
            conflict.call_ref,
            conflict.memory_id,
            conflict.expected_revision,
            conflict.actual_revision
        )
    } else {
        conflict.history_id.clone()
    }
}
fn overwritten_key(record: &OverwrittenRecord) -> String {
    let field = match &record.field {
        OverwrittenField::Name { .. } => "name".into(),
        OverwrittenField::Instructions { .. } => "instructions".into(),
        OverwrittenField::Model { provider, .. } => format!("model:{provider:?}"),
        OverwrittenField::ReasoningEffort { provider, .. } => {
            format!("reasoning_effort:{provider:?}")
        }
        OverwrittenField::Skills { .. } => "skills".into(),
        OverwrittenField::Avatar { .. } => "avatar".into(),
    };
    format!("{}:{field}", record.change_id)
}
fn history_record_keys(c: &Conversation, section: S) -> Vec<String> {
    match section {
        S::Messages => vec![],
        S::Works => c.works.iter().map(|row| row.work_id.clone()).collect(),
        S::Changes => c.changes.iter().map(|row| row.change_id.clone()).collect(),
        S::Questions => c
            .questions
            .iter()
            .map(|row| row.question_id.clone())
            .collect(),
        S::Sessions => c
            .sessions
            .iter()
            .map(|row| format!("{}:{}", row.node, row.session_id))
            .collect(),
        S::Jobs => c
            .scheduler
            .jobs
            .iter()
            .map(|row| row.trigger.clone())
            .collect(),
        S::ScopeJobs => c
            .scheduler
            .scope_jobs
            .iter()
            .map(|row| row.trigger.clone())
            .collect(),
        S::OwnerJobs => c
            .scheduler
            .remote
            .iter()
            .flat_map(|remote| remote.jobs.iter())
            .map(|row| row.trigger.clone())
            .collect(),
        S::OwnerScopeJobs => c
            .scheduler
            .remote
            .iter()
            .flat_map(|remote| remote.scope_jobs.iter())
            .map(|row| row.trigger.clone())
            .collect(),
        S::OwnerMemoryConflicts => c
            .scheduler
            .remote
            .iter()
            .flat_map(|remote| remote.memory_conflicts.iter().flatten())
            .map(conflict_key)
            .collect(),
        S::Checks => c.checks.iter().map(|row| row.check_id.clone()).collect(),
        S::Deliveries => c
            .deliveries
            .iter()
            .map(|row| match row {
                DeliveryControl::StopWaiting { work_id, .. }
                | DeliveryControl::StopTimedOut { work_id, .. } => format!("stop:{work_id}"),
                DeliveryControl::DispatchWaiting { work_id, .. }
                | DeliveryControl::DispatchRejected { work_id, .. }
                | DeliveryControl::DispatchUnverified { work_id, .. } => {
                    format!("dispatch:{work_id}")
                }
            })
            .collect(),
        S::Unanswered => c.scheduler.unanswered.clone(),
        S::Skills => c
            .bot_settings
            .skills
            .iter()
            .map(|row| row.skill_id.clone())
            .collect(),
        S::Schedules => c
            .bot_settings
            .schedules
            .iter()
            .map(|row| row.schedule_id.clone())
            .collect(),
        S::Memory => c
            .bot_settings
            .memory
            .iter()
            .map(|row| row.memory_id.clone())
            .collect(),
        S::MemoryConflicts => c
            .bot_settings
            .memory_conflicts
            .iter()
            .flatten()
            .map(conflict_key)
            .collect(),
        S::MemorySyncConflicts => c
            .bot_settings
            .memory_sync_conflicts
            .items
            .iter()
            .map(|row| format!("{}:{}", row.memory_id, row.losing_revision))
            .collect(),
        S::Overwritten => c.overwritten.iter().map(overwritten_key).collect(),
    }
}

impl Desktop {
    pub(crate) fn history_coverage(&self, section: S, cx: &mut Context<Self>) -> AnyElement {
        let key = history_key(section);
        let mut row = div()
            .flex()
            .flex_wrap()
            .items_center()
            .gap_2()
            .px_4()
            .py_2()
            .text_sm();
        if let Some(coverage) = self.snapshot.conversation.history.get(&key) {
            row = row.child(format!(
                "{}：{}",
                history_label(section),
                if !self.snapshot.online {
                    if coverage.complete {
                        "上次已读取至起点，当前状态待连接确认"
                    } else {
                        "上次尚有更早记录，连接后继续"
                    }
                } else if coverage.complete {
                    "已读取全部历史"
                } else {
                    "尚有更早记录"
                }
            ));
            if !coverage.complete {
                if let Some(conversation_id) = self.conversation_id() {
                    row = row.child(
                        self.button(
                            SharedString::from(format!("load-history-{key}")),
                            if self.snapshot.history_loading == Some(section) {
                                "正在读取…"
                            } else {
                                "读取更早记录"
                            },
                            Command::LoadHistory {
                                conversation_id,
                                section,
                            },
                            cx,
                        )
                        .disabled(!self.snapshot.online || self.snapshot.history_loading.is_some()),
                    );
                }
            }
        } else {
            row = row.child(format!(
                "{}：{}",
                history_label(section),
                if self.snapshot.cached {
                    "离线缓存未保留完整范围，连接后重新读取"
                } else {
                    "尚未确认历史范围"
                }
            ));
        }
        if let Some(error) = &self.snapshot.history_error {
            row = row.child(div().text_color(self.palette().danger).child(error.clone()));
        }
        row.into_any_element()
    }
    pub(crate) fn conversation_history(&self, section: S, cx: &mut Context<Self>) -> AnyElement {
        let mut page = self.page().child(self.bot_context_header(cx));
        if !self.selected_conversation_loaded() {
            return page
                .child("请先打开一个Bot的对话，再读取历史。")
                .into_any_element();
        }
        let mut tabs = div().flex().flex_wrap().gap_2();
        for choice in S::ALL {
            let generation = self.snapshot.generation;
            let network = self.snapshot.network.clone();
            tabs = tabs.child(
                Button::new(SharedString::from(format!(
                    "history-section-{}",
                    history_key(choice)
                )))
                .label(history_label(choice))
                .on_click(cx.listener(move |this, _, _, cx| {
                    if this.same_view_scope(generation, &network) {
                        this.navigate(Route::History(choice), cx);
                    }
                })),
            );
        }
        page = page.child(tabs).child(self.history_coverage(section, cx));
        if matches!(
            section,
            S::OwnerJobs | S::OwnerScopeJobs | S::OwnerMemoryConflicts
        ) {
            page = page.child(
                self.snapshot
                    .conversation
                    .scheduler
                    .remote
                    .as_ref()
                    .map(|remote| {
                        format!(
                            "拥有者 {} · {}",
                            remote.owner,
                            if remote.reachable {
                                "可达，执行状态以拥有者为准"
                            } else {
                                "不可达，状态待核对"
                            }
                        )
                    })
                    .unwrap_or_else(|| "尚未读取拥有者状态".into()),
            );
        }
        if section == S::Messages {
            if let Some(id) = self.conversation_id() {
                page = page.child(self.nav(
                    "history-open-messages",
                    "打开消息时间线",
                    Route::Chat(id),
                    cx,
                ));
            }
            return page.into_any_element();
        }
        div()
            .flex()
            .flex_col()
            .flex_1()
            .min_h_0()
            .w_full()
            .child(page)
            .child(
                list(
                    self.history_list.list.clone(),
                    cx.processor(move |this, index, _, cx| {
                        this.conversation_history_row(section, index, cx)
                    }),
                )
                .flex_1()
                .min_h_0()
                .w_full(),
            )
            .into_any_element()
    }
    pub(crate) fn prepare_history_list(&mut self) {
        let Route::History(section) = &self.route else {
            return;
        };
        let section = *section;
        let scope = format!(
            "{:?}",
            (
                &self.snapshot.network,
                &self.snapshot.client_id,
                &self.selected,
                self.snapshot.generation,
                &self.snapshot.conversation.history_authority,
                section
            )
        );
        let keys = history_record_keys(&self.snapshot.conversation, section);
        self.history_list.replace(scope, keys);
    }
    pub(crate) fn conversation_history_row(
        &self,
        section: S,
        index: usize,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let mut page = div().flex().flex_col().gap_3().px_6().py_2();
        let c = &self.snapshot.conversation;
        let id = self.conversation_id().unwrap_or_default();
        match section {
            S::Messages => {
                page = page.child(self.nav(
                    "history-open-messages",
                    "打开消息时间线",
                    Route::Chat(id),
                    cx,
                ));
            }
            S::Works => {
                for work in c.works.iter().skip(index).take(1) {
                    let work_id = work.work_id.clone();
                    let conversation_id = id.clone();
                    page = page.child(
                        self.row(
                            work.title.clone(),
                            format!(
                                "{} · {}\n{}\n\n{}",
                                work.state, work.target_node, work.summary, work.result_text
                            ),
                        )
                        .child(
                            Button::new(SharedString::from(format!("history-work-{work_id}")))
                                .label("打开完整工作记录")
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.navigate(
                                        Route::Work {
                                            conversation_id: conversation_id.clone(),
                                            work_id: work_id.clone(),
                                        },
                                        cx,
                                    )
                                })),
                        ),
                    );
                }
            }
            S::Changes => {
                for change in c.changes.iter().skip(index).take(1) {
                    let mut row = self.row(
                        format!("变更 {}", change.change_id),
                        format!("时间 {} · 事件 {}", change.created_at_ms, change.event_hash),
                    );
                    if let Some(cause) = &change.cause {
                        row = row.child(format!(
                            "触发消息 {} · 对话 {}",
                            cause.message_id, cause.conversation_id
                        ));
                    }
                    for (name, value) in change_fields(&change.change) {
                        row = row.child(self.row(name, value));
                    }
                    page = page.child(row);
                }
            }
            S::Questions => {
                for question in c.questions.iter().skip(index).take(1) {
                    let mut row =
                        self.row(question.title.clone(), question.text.clone())
                            .child(format!(
                                "问题 {} · {}",
                                question.question_id,
                                question.closed.as_deref().unwrap_or(
                                    if question.answer.is_some() {
                                        "已回复"
                                    } else {
                                        "等待回复"
                                    }
                                )
                            ));
                    for choice in &question.choices {
                        row = row.child(format!("选项：{}", choice.label));
                    }
                    if question.answer.is_none() && question.closed.is_none() {
                        row = row.child(self.open_editor(
                            SharedString::from(format!("history-answer-{}", question.question_id)),
                            "输入回复",
                            crate::views::forms::Editor::Answer(
                                question.question_id.clone(),
                                question.conversation_id.clone(),
                            ),
                            std::collections::BTreeMap::new(),
                            cx,
                        ));
                    } else if let Some(answer) = &question.answer {
                        if let Some(text) = answer
                            .as_str()
                            .or_else(|| answer.get("text").and_then(serde_json::Value::as_str))
                        {
                            row = row.child(text.to_owned());
                        }
                    }
                    page = page.child(row);
                }
            }
            S::Sessions => {
                for session in c.sessions.iter().skip(index).take(1) {
                    page = page.child(self.row(
                        format!("{:?} · {}", session.agent, session.session_id),
                        format!(
                            "设备 {} · {} · {}\n{}",
                            session.node,
                            session.cwd,
                            if session.muted { "已静默" } else { "可见" },
                            session.summary
                        ),
                    ));
                }
            }
            S::Jobs | S::OwnerJobs => {
                let remote = section == S::OwnerJobs;
                let jobs = if remote {
                    c.scheduler
                        .remote
                        .as_ref()
                        .map(|r| r.jobs.as_slice())
                        .unwrap_or(&[])
                } else {
                    &c.scheduler.jobs
                };
                for job in jobs.iter().skip(index).take(1) {
                    page = page.child(self.row(
                        format!("{} · {}", scope_label(&job.scope), job.trigger),
                        format!(
                                "{}{}",
                                job.state,
                                job.failure
                                    .as_ref()
                                    .map(|failure| format!("\n{}", failure.detail))
                                    .unwrap_or_default()
                            ),
                    ));
                }
            }
            S::ScopeJobs | S::OwnerScopeJobs => {
                let jobs = if section == S::OwnerScopeJobs {
                    c.scheduler
                        .remote
                        .as_ref()
                        .map(|r| r.scope_jobs.as_slice())
                        .unwrap_or(&[])
                } else {
                    &c.scheduler.scope_jobs
                };
                for job in jobs.iter().skip(index).take(1) {
                    page = page.child(self.row(
                        scope_label(&job.scope),
                        format!(
                                "{} · 触发 {} · 对话 {}{}",
                                job.state,
                                job.trigger,
                                job.conversation_id,
                                job.failure
                                    .as_ref()
                                    .map(|failure| format!("\n{}", failure.detail))
                                    .unwrap_or_default()
                            ),
                    ));
                }
            }
            S::OwnerMemoryConflicts | S::MemoryConflicts => {
                let conflicts = if section == S::OwnerMemoryConflicts {
                    c.scheduler
                        .remote
                        .as_ref()
                        .and_then(|r| r.memory_conflicts.as_deref())
                        .unwrap_or(&[])
                } else {
                    c.bot_settings.memory_conflicts.as_deref().unwrap_or(&[])
                };
                for conflict in conflicts.iter().skip(index).take(1) {
                    page = page.child(self.row(
                        format!("记忆 {}", conflict.memory_id),
                        format!(
                            "预期 {} · 当前 {}\n{}",
                            conflict.expected_revision, conflict.actual_revision, conflict.reason
                        ),
                    ));
                }
            }
            S::Checks => {
                for check in c.checks.iter().skip(index).take(1) {
                    page = page.child(
                        self.row(
                            format!("进程核对 {}", check.check_id),
                            format!(
                                "{}\n执行 {} · 触发 {}",
                                check.detail, check.attempt_id, check.trigger
                            ),
                        )
                        .child(self.ask(
                            SharedString::from(format!("history-check-{}", check.check_id)),
                            "我已核实进程停止",
                            Command::ResolveCheck(check.clone()),
                            cx,
                        )),
                    );
                }
            }
            S::Deliveries => {
                for delivery in c.deliveries.iter().skip(index).take(1) {
                    let (title, detail) = match delivery {
                        DeliveryControl::StopWaiting {
                            work_id,
                            target_node,
                        } => (
                            format!("工作 {work_id} 停止待确认"),
                            format!("目标设备 {target_node}"),
                        ),
                        DeliveryControl::StopTimedOut {
                            work_id,
                            target_node,
                        } => (
                            format!("工作 {work_id} 停止确认超时"),
                            format!("目标设备 {target_node}，进程结果尚未知"),
                        ),
                        DeliveryControl::DispatchWaiting {
                            work_id,
                            target_node,
                            cwd,
                            title,
                            prompt,
                            reason,
                            ..
                        }
                        | DeliveryControl::DispatchRejected {
                            work_id,
                            target_node,
                            cwd,
                            title,
                            prompt,
                            reason,
                            ..
                        }
                        | DeliveryControl::DispatchUnverified {
                            work_id,
                            target_node,
                            cwd,
                            title,
                            prompt,
                            reason,
                            ..
                        } => (
                            format!("{} · {work_id}", title.as_deref().unwrap_or("工作投递")),
                            format!(
                                "{} · {target_node} · {cwd}\n{}\n{}",
                                match delivery {
                                    DeliveryControl::DispatchWaiting { .. } => "等待确认",
                                    DeliveryControl::DispatchRejected { .. } => "已拒绝",
                                    _ => "未验证",
                                },
                                prompt.as_deref().unwrap_or(""),
                                reason.as_deref().unwrap_or("")
                            ),
                        ),
                    };
                    page = page.child(self.row(title, detail));
                }
            }
            S::Unanswered => {
                for trigger in c.scheduler.unanswered.iter().skip(index).take(1) {
                    page = page.child(self.row(
                        "尚未完成回复",
                        format!("原消息 {trigger}，先核对执行记录再继续。"),
                    ));
                }
            }
            S::Skills => {
                for skill in c.bot_settings.skills.iter().skip(index).take(1) {
                    let body = skill.body.clone();
                    page = page.child(
                        self.row(
                            skill.name.clone(),
                            format!("{}\n技能 {}", skill.description, skill.skill_id),
                        )
                        .child(skill.body.clone())
                        .child(
                            Button::new(SharedString::from(format!(
                                "copy-skill-body-{}",
                                skill.skill_id
                            )))
                            .label("复制技能正文")
                            .on_click(move |_, _, cx| {
                                cx.write_to_clipboard(ClipboardItem::new_string(body.clone()))
                            }),
                        ),
                    );
                }
            }
            S::Schedules => {
                for schedule in c.bot_settings.schedules.iter().skip(index).take(1) {
                    page = page.child(self.row(
                        schedule.name.clone(),
                        format!(
                                "{} · 首次 {} · 下次 {}\n{}",
                                if schedule.enabled { "启用" } else { "暂停" },
                                schedule.first_run_at_ms,
                                schedule
                                    .next_run_at_ms
                                    .map(|at| at.to_string())
                                    .unwrap_or_else(|| "未排定".into()),
                                schedule.prompt
                            ),
                    ));
                }
            }
            S::Memory => {
                for memory in c.bot_settings.memory.iter().skip(index).take(1) {
                    page = page.child(self.row(
                        memory.text.clone(),
                        format!(
                            "记忆 {} · 修订 {} · 来源事件 {}",
                            memory.memory_id,
                            memory.metadata.revision,
                            memory.metadata.source_event_hash
                        ),
                    ));
                }
            }
            S::MemorySyncConflicts => {
                for conflict in c
                    .bot_settings
                    .memory_sync_conflicts
                    .items
                    .iter()
                    .skip(index)
                    .take(1)
                {
                    page = page.child(self.row(
                        format!("记忆 {}", conflict.memory_id),
                        format!(
                            "被覆盖版本 {} · 当前版本 {}\n{}",
                            conflict.losing_revision, conflict.winning_revision, conflict.reason
                        ),
                    ));
                }
            }
            S::Overwritten => {
                for (index, record) in c.overwritten.iter().enumerate().skip(index).take(1) {
                    let (name, winner) = overwritten_words(&record.field);
                    let mut row = self.row(
                        format!("{name}被其他设备覆盖"),
                        format!(
                            "设备 {} · 当前值\n{winner}\n原始变更 {}",
                            record.by, record.change_id
                        ),
                    );
                    if let Some(original) = c
                        .changes
                        .iter()
                        .find(|change| change.change_id == record.change_id)
                    {
                        if record.original_after(original).is_ok() {
                            row = row.child(self.ask(
                                SharedString::from(format!("restore-profile-{index}")),
                                "改回我的原值",
                                Command::RestoreProfile {
                                    conversation_id: id.clone(),
                                    record: record.clone(),
                                },
                                cx,
                            ));
                        } else {
                            row = row.child("原始变更未包含此字段，不能推断原值。");
                        }
                    } else {
                        row = row
                            .child("原始变更尚未读取，请在配置变更中继续读取更早记录。")
                            .child(
                                Button::new(SharedString::from(format!(
                                    "load-original-change-{index}"
                                )))
                                .label("查看配置变更")
                                .on_click(cx.listener(
                                    |this, _, _, cx| this.navigate(Route::History(S::Changes), cx),
                                )),
                            );
                    }
                    page = page.child(row);
                }
            }
        }
        page.into_any_element()
    }
}
