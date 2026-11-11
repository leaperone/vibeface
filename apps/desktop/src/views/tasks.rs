use crate::{
    app::Desktop,
    ui::Route,
    views::{backend::Command, forms::Editor, records::HistoryBody},
};
use gpui_kit::{component::button::Button, *};
use std::collections::BTreeMap;
use vibeface_protocol::workspace::{InputTarget, TaskAction, WorkspaceCommand};
impl Desktop {
    pub(crate) fn task_detail(&self, cx: &mut Context<Self>) -> AnyElement {
        let Route::Task(task_id) = &self.route else {
            return self.page().into_any_element();
        };
        let mut page = self
            .page()
            .child(self.nav("task-back", "所有任务", Route::Tasks, cx));
        let Some(task) = self
            .snapshot
            .tasks
            .iter()
            .find(|task| &task.task_id == task_id)
        else {
            return page
                .child("当前连接尚未读取此任务，请刷新后核对。")
                .into_any_element();
        };
        page = page
            .child(self.row(
                task.title.clone(),
                format!(
                    "{} · 修订 {} · 负责Bot {}",
                    task.state, task.revision, task.owner_bot
                ),
            ))
            .child(self.row("任务交代", task.request.clone()))
            .child(self.row("预期结果", task.expected_result.clone()));
        if let Some(reason) = &task.waiting_reason {
            page = page.child(self.row("等待原因", reason.clone()));
        }
        for (label, id) in [
            ("根任务", Some(&task.root_task_id)),
            ("父任务", task.parent_task_id.as_ref()),
        ] {
            if let Some(id) = id
                && id != task_id
                && !id.is_empty()
            {
                if self.snapshot.tasks.iter().any(|other| &other.task_id == id) {
                    page = page.child(self.task_link(label, id, cx));
                } else {
                    page = page.child(self.row(label, format!("{id} · 尚未读取")));
                }
            }
        }
        for id in &task.dependencies {
            page = page.child(
                if self.snapshot.tasks.iter().any(|other| &other.task_id == id) {
                    div().child(self.task_link("依赖任务", id, cx))
                } else {
                    self.row("依赖任务", format!("{id} · 尚未读取，状态未知"))
                },
            );
        }
        page = page
            .child(self.open_editor(
                "task-revise",
                "修改任务",
                Editor::EditTask(task.task_id.clone(), task.revision),
                BTreeMap::from([
                    ("title", task.title.clone()),
                    ("owner_bot", task.owner_bot.clone()),
                    ("request", task.request.clone()),
                    ("expected_result", task.expected_result.clone()),
                    ("dependencies", task.dependencies.join(",")),
                ]),
                cx,
            ))
            .child(self.open_editor(
                "task-assign",
                "分派执行",
                Editor::AssignTask(task.task_id.clone(), task.revision),
                BTreeMap::from([
                    ("bot_id", task.owner_bot.clone()),
                    ("automatic", "false".into()),
                ]),
                cx,
            ));
        for (label, action) in [
            ("继续", TaskAction::Continue),
            ("停止", TaskAction::Stop),
            ("延后", TaskAction::Defer),
            ("取消", TaskAction::Cancel),
        ] {
            page = page.child(self.button(
                label,
                label,
                Command::Workspace(WorkspaceCommand::ControlTask {
                    task_id: task.task_id.clone(),
                    expected_revision: task.revision,
                    action,
                }),
                cx,
            ));
        }
        for assignment in self
            .snapshot
            .assignments
            .iter()
            .filter(|assignment| &assignment.task_id == task_id)
        {
            let mut row = self.row(
                "执行分派",
                format!(
                    "{} · {:?} · 启动修订 {} · 设备 {} · Bot {}",
                    assignment.assignment_id,
                    assignment.state,
                    assignment.started_revision,
                    assignment.execution_node,
                    assignment.bot_id
                ),
            );
            if let Some(result) = &assignment.result {
                row = row.child(result.clone());
            }
            if let Some(work_id) = &assignment.work_id
                && let Some(bot) = self
                    .snapshot
                    .bots
                    .iter()
                    .find(|bot| bot.bot_id == assignment.bot_id)
            {
                row = row.child(self.task_work_link(&bot.conversation_id, work_id, cx));
            }
            page = page.child(row);
        }
        page = page.child(self.button(
            "task-history-refresh",
            "重新读取任务活动",
            Command::History {
                task_id: Some(task_id.clone()),
                channel_id: None,
            },
            cx,
        ));
        if let Some(history) = self.snapshot.channel_history.as_ref().filter(|history| {
            history.task_id.as_ref() == Some(task_id)
                && history.network_id == self.snapshot.network.clone().unwrap_or_default()
        }) {
            page = page.child(self.row(
                "因果活动",
                format!(
                    "读取上限 {} 条 · {}",
                    history.coverage.limit,
                    if history.coverage.truncated {
                        "历史已截断"
                    } else {
                        "当前范围已读"
                    }
                ),
            ));
            for event in &history.events {
                let mut row = self.row(
                    format!("{} · {}", event.created_at_ms, event.body.kind()),
                    format!("作用域 {} · 事件 {}", event.scope_id, event.event_hash),
                );
                match &event.body {
                    HistoryBody::InputPosted { input } => {
                        row = row.child(self.record_text(
                            &format!("history:{}:body", event.event_hash),
                            &input.text,
                        ))
                    }
                    HistoryBody::MessagePosted { text, .. } => {
                        row = row.child(
                            self.record_text(&format!("history:{}:body", event.event_hash), text),
                        )
                    }
                    HistoryBody::WorkDispatched {
                        work_id,
                        title,
                        prompt,
                        ..
                    } => {
                        row =
                            row.child(title.clone())
                                .child(self.record_text(
                                    &format!("history:{}:body", event.event_hash),
                                    prompt,
                                ))
                                .child(self.task_work_link(&event.scope_id, work_id, cx))
                    }
                    HistoryBody::WorkProgress {
                        work_id,
                        phase,
                        summary,
                        ..
                    } => {
                        row = row
                            .child(format!("{phase:?}"))
                            .child(self.record_text(
                                &format!("history:{}:body", event.event_hash),
                                summary,
                            ))
                            .child(self.task_work_link(&event.scope_id, work_id, cx))
                    }
                    HistoryBody::WorkFinished {
                        work_id,
                        outcome,
                        result_text,
                        ..
                    } => {
                        row = row
                            .child(format!("{outcome:?}"))
                            .child(self.record_text(
                                &format!("history:{}:body", event.event_hash),
                                result_text,
                            ))
                            .child(self.task_work_link(&event.scope_id, work_id, cx))
                    }
                    HistoryBody::WorkspaceCommanded { command, .. } => {
                        row = row.child(match command {
                            WorkspaceCommand::CreateTask { title, .. } => {
                                format!("创建任务 {title}")
                            }
                            WorkspaceCommand::ReviseTask {
                                expected_revision, ..
                            } => format!("修改任务，预期修订 {expected_revision}"),
                            WorkspaceCommand::ControlTask { action, .. } => {
                                format!("任务动作 {action:?}")
                            }
                            WorkspaceCommand::AssignTask { execution_node, .. } => {
                                format!("分派到 {execution_node}")
                            }
                            _ => "共享操作已记录".into(),
                        })
                    }
                };
                if let Some(scope) = &event.attachment_conversation {
                    for file in event.body.attachments() {
                        row = row.child(self.record_attachment(&event.event_hash, scope, file, cx));
                    }
                }
                page = page.child(row);
            }
        } else {
            page = page.child("尚未读取此任务的活动，不使用其他任务的缓存。");
        }
        if let Some(bot) = self
            .snapshot
            .bots
            .iter()
            .find(|bot| bot.bot_id == task.owner_bot)
        {
            let target = if let Some(channel_id) = &task.channel_id {
                InputTarget::ChannelTask {
                    channel_id: channel_id.clone(),
                    task_id: task_id.clone(),
                    bot_id: bot.bot_id.clone(),
                }
            } else {
                InputTarget::Task {
                    task_id: task_id.clone(),
                }
            };
            page = page.child(self.record_composer(
                bot.conversation_id.clone(),
                target,
                Some(task.revision),
                true,
                cx,
            ));
        }
        page.child(self.record_artifact_preview(cx))
            .into_any_element()
    }
    fn task_link(&self, label: &str, id: &str, cx: &mut Context<Self>) -> Button {
        let id = id.to_owned();
        Button::new(SharedString::from(format!("task-link-{id}")))
            .label(format!("{label} · {id}"))
            .on_click(cx.listener(move |this, _, _, cx| this.navigate(Route::Task(id.clone()), cx)))
    }
    fn task_work_link(&self, conversation: &str, id: &str, cx: &mut Context<Self>) -> Button {
        let conversation = conversation.to_owned();
        let id = id.to_owned();
        Button::new(SharedString::from(format!("task-work-{id}")))
            .label("查看执行工作")
            .on_click(cx.listener(move |this, _, _, cx| {
                this.navigate(
                    Route::Work {
                        conversation_id: conversation.clone(),
                        work_id: id.clone(),
                    },
                    cx,
                )
            }))
    }
}
