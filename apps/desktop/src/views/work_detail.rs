use crate::{
    app::Desktop,
    ui::Route,
    views::{
        backend::Command,
        forms::Editor,
        records::{HistoryBody, WorkOutcome, WorkState},
    },
};
use gpui_kit::{
    component::{Disableable, button::Button},
    *,
};
use std::collections::BTreeMap;
use vibeface_client::ResolveOutcome;
use vibeface_protocol::workspace::{InputControl, InputTarget, WorkspaceCommand};

impl Desktop {
    pub(crate) fn work_detail(&self, cx: &mut Context<Self>) -> AnyElement {
        let Route::Work {
            conversation_id,
            work_id,
        } = &self.route
        else {
            return self.page().into_any_element();
        };
        let refresh = Command::WorkDetail {
            conversation_id: conversation_id.clone(),
            work_id: work_id.clone(),
        };
        let mut page = self
            .page()
            .child(self.nav(
                "work-back",
                "返回对话",
                Route::Chat(conversation_id.clone()),
                cx,
            ))
            .child(self.button("work-refresh", "重新读取工作", refresh, cx));
        let Some(detail) = self.snapshot.work_detail.as_ref().filter(|detail| {
            self.snapshot.network.as_deref() == Some(detail.network_id.as_str())
                && detail.work.conversation_id == *conversation_id
                && detail.work.work_id == *work_id
        }) else {
            return page
                .child(if self.snapshot.online {
                    "尚未读取此工作的共享记录。读取失败时可重新读取。"
                } else {
                    "尚未连接，工作状态未知。连接后重新读取。"
                })
                .into_any_element();
        };
        let work = &detail.work;
        let node = self
            .snapshot
            .nodes
            .iter()
            .find(|node| node.device_id == work.target_node);
        let active = matches!(
            work.state,
            WorkState::Accepted | WorkState::Running | WorkState::Interrupting
        );
        let uncertain = work.outcome == Some(WorkOutcome::Unverified)
            || (active && (!self.snapshot.online || node.is_none_or(|n| !n.online || n.revoked)));
        let approval_pending = active
            && self.snapshot.questions.iter().any(|q| {
                q.conversation_id == *conversation_id
                    && q.answer.is_none()
                    && q.closed.is_none()
                    && q.about.as_ref().is_some_and(|about| {
                        about.node == work.target_node
                            && Some(about.session_id.as_str()) == work.thread_id.as_deref()
                            && about.request_id.as_deref().is_some_and(|id| !id.is_empty())
                    })
            });
        page = page
            .child(self.row(
                work.title.clone(),
                if uncertain {
                    "执行结果待核实".into()
                } else if approval_pending {
                    "需要你同意或回复".into()
                } else {
                    format!(
                        "{} · {}",
                        work_state_label(work.state),
                        work.outcome.map(outcome_label).unwrap_or("尚无终态")
                    )
                },
            ))
            .child(format!(
                "来源：{}。完整执行历史{}。",
                detail.coverage.source,
                if detail.coverage.complete {
                    "已提供"
                } else {
                    "未提供"
                }
            ));
        if !self.snapshot.online {
            page = page.child("当前显示缓存记录。连接中断期间的进度和停止结果尚未确认。");
        }
        if self.snapshot.local_session {
            page = page.child("当前为本机管理会话。加入个人网络后才能发送签名输入。");
        }
        page = page
            .child(
                self.row(
                    "执行设备",
                    node.map(|n| {
                        format!(
                            "{} · {}{}",
                            n.name,
                            if !self.snapshot.online {
                                "当前状态未确认"
                            } else if n.online {
                                "在线"
                            } else {
                                "离线"
                            },
                            if n.revoked { " · 已撤销" } else { "" }
                        )
                    })
                    .unwrap_or_else(|| format!("{} · 尚未读取设备状态", work.target_node)),
                ),
            )
            .child(self.row(
                "请求的模型",
                work.model.as_deref().unwrap_or("设备默认模型"),
            ))
            .child(self.row(
                "接收时的 Provider 与模型",
                format!(
                        "{:?} · {}",
                        work.resolved.provider,
                        work.resolved
                            .model
                            .as_deref()
                            .unwrap_or("接收时的设备默认模型")
                    ),
            ))
            .child(self.row("工作目录", work.cwd.clone()))
            .child(self.row("原始交代", "发送给执行设备的原文"))
            .child(self.record_text(&format!("work:{}:prompt", work.work_id), &work.prompt));
        if !work.summary.is_empty() {
            page = page
                .child(self.row(
                    "共享进度",
                    format!(
                            "阶段：{}",
                            work.phase
                                .map(|p| format!("{p:?}"))
                                .unwrap_or_else(|| "尚未上报".into())
                        ),
                ))
                .child(self.record_text(&format!("work:{}:summary", work.work_id), &work.summary));
        }
        let live_key = format!(
            "native:{}:{}:{}",
            work.conversation_id, work.target_node, work.work_id
        );
        for progress in self.snapshot.live.iter().filter(|item| {
            item.key == live_key && item.conversation_id == work.conversation_id && active
        }) {
            page = page.child(self.row(
                "实时执行状态与工具",
                format!("{} · 序号 {}", progress.status, progress.sequence),
            ));
            if !progress.text.is_empty() {
                page = page.child(progress.text.clone());
            }
            for tool in &progress.tools {
                page = page
                    .child(self.row(
                        format!("工具 · {}", tool.name),
                        format!("{} · 序号 {}", tool.status, tool.sequence),
                    ))
                    .child(tool.text.clone());
            }
            if progress.tools_omitted > 0 {
                page = page.child(format!(
                    "实时工具记录已省略 {} 条；共享历史不包含完整工具执行记录。",
                    progress.tools_omitted
                ));
            }
        }
        if let Some(failure) = work.failure {
            page = page.child(self.row("执行失败类型", format!("{failure:?}")));
        }
        if let Some(reason) = &work.failure_reason {
            page = page.child(self.row("执行说明", reason.clone()));
        }
        if !work.result_text.is_empty() {
            page = page
                .child(self.row(
                    if matches!(
                        work.outcome,
                        Some(
                            WorkOutcome::Failed | WorkOutcome::Interrupted | WorkOutcome::Rejected
                        )
                    ) {
                        "结束前的输出"
                    } else {
                        "共享结果"
                    },
                    "结果以执行记录的终态为准",
                ))
                .child(
                    self.record_text(&format!("work:{}:result", work.work_id), &work.result_text),
                );
        } else if work.outcome.is_some() {
            page = page.child("此终态没有结果文本。");
        }
        if uncertain {
            page = page
                .child("设备尚未证明实际结果。先查询原记录；重新执行可能重复产生副作用。")
                .child(
                    self.ask(
                        "work-settle-completed",
                        "我已核实：已完成",
                        Command::ResolveWork {
                            conversation_id: conversation_id.clone(),
                            work_id: work_id.clone(),
                            outcome: ResolveOutcome::Completed,
                        },
                        cx,
                    )
                    .disabled(!self.snapshot.writable || !self.snapshot.online),
                )
                .child(
                    self.ask(
                        "work-settle-interrupted",
                        "我已核实：未完成",
                        Command::ResolveWork {
                            conversation_id: conversation_id.clone(),
                            work_id: work_id.clone(),
                            outcome: ResolveOutcome::Interrupted,
                        },
                        cx,
                    )
                    .disabled(!self.snapshot.writable || !self.snapshot.online),
                );
        } else if active && work.state != WorkState::Interrupting {
            page = page.child(
                self.ask(
                    "work-stop",
                    "请求停止工作",
                    Command::StopWork {
                        conversation_id: conversation_id.clone(),
                        work_id: work_id.clone(),
                    },
                    cx,
                )
                .disabled(!self.snapshot.can_control),
            );
        }
        if let (Some(thread), Some(turn)) = (&work.thread_id, &work.turn_id) {
            page = page.child(self.row("执行会话与运行", format!("线程 {thread}\n运行 {turn}")));
            if self
                .snapshot
                .conversation
                .conversation
                .as_ref()
                .is_some_and(|scope| scope.conversation_id == *conversation_id)
            {
                let sharing = self
                    .snapshot
                    .conversation
                    .works
                    .iter()
                    .filter(|other| {
                        other.work_id != work.work_id
                            && other.outcome.is_none()
                            && other.thread_id.as_ref() == Some(thread)
                            && other.turn_id.as_ref() == Some(turn)
                    })
                    .collect::<Vec<_>>();
                if !sharing.is_empty() {
                    page = page.child("停止此运行也会停止以下共享线程与运行的工作：");
                    for other in sharing {
                        page = page.child(self.nav_work_link(
                            &other.title,
                            conversation_id,
                            &other.work_id,
                            cx,
                        ));
                    }
                }
            }
        }
        for session in self
            .snapshot
            .conversation
            .sessions
            .iter()
            .filter(|session| {
                session.node == work.target_node
                    && session.agent == work.resolved.provider
                    && work.thread_id.as_deref() == Some(session.session_id.as_str())
            })
        {
            page = page.child(self.row(
                "设备上的会话记录",
                format!(
                    "{} · {} · {} · {}",
                    session.session_id,
                    session.cwd,
                    session.summary,
                    if session.muted {
                        "已静音"
                    } else {
                        "未静音"
                    }
                ),
            ));
        }
        for (label, linked) in [
            ("所属上级工作", &work.parent),
            ("继续自工作", &work.continues),
        ] {
            if let Some(linked) = linked {
                if self
                    .snapshot
                    .conversation
                    .works
                    .iter()
                    .any(|item| item.work_id == *linked)
                    && self
                        .snapshot
                        .conversation
                        .conversation
                        .as_ref()
                        .is_some_and(|c| c.conversation_id == *conversation_id)
                {
                    page = page.child(self.nav_work_link(label, conversation_id, linked, cx));
                } else {
                    page = page
                        .child(self.row(label, format!("{linked} · 尚未读取归属，不能直接跳转")));
                }
            }
        }
        page = page
            .child(self.row("来源事件", work.source_events.join("\n")))
            .child(self.row(
                "工作标识",
                format!(
                    "{}\n调度触发 {}\n调用 {}\n调度设备 {}\n类型 {:?}",
                    work.work_id, work.trigger, work.call_ref, work.dispatcher, work.kind
                ),
            ));
        page = page.child(self.button(
            "work-context",
            "读取此工作上下文",
            Command::ScopeContext {
                conversation_id: conversation_id.clone(),
                target: InputTarget::Work {
                    work_id: work_id.clone(),
                },
            },
            cx,
        ));
        if let Some(target) = &work.target {
            if let InputTarget::ChannelTask {
                channel_id,
                task_id,
                ..
            } = target
            {
                if self.snapshot.tasks.iter().any(|task| {
                    task.task_id == *task_id && task.channel_id.as_ref() == Some(channel_id)
                }) {
                    page = page.child(self.nav(
                        "work-channel",
                        "打开来源频道",
                        Route::Channel(channel_id.clone()),
                        cx,
                    ));
                }
            }
        }
        for queued in self.snapshot.inputs.iter().filter(|input| !input.cancelled && input.conversation_id == *conversation_id && matches!(&input.input.route.target,InputTarget::Work{work_id:id} if id == work_id)) {
            let mut row = self.row(if queued.deferred {"已延后补充"} else {"等待执行的补充"}, queued.input.text.clone());
            for (label,action) in [("释放",InputControl::Release),("取消",InputControl::Cancel),("移到末尾",InputControl::Move{before_message_id:None})] {
                row=row.child(self.button(SharedString::from(format!("work-queue-{}-{label}",queued.input.message_id)),label,Command::Workspace(WorkspaceCommand::ControlInput{message_id:queued.input.message_id.clone(),expected_event:queued.control_event.clone(),action}),cx).disabled(!self.snapshot.writable));
            }
            page=page.child(row.child(self.open_editor(SharedString::from(format!("work-edit-input-{}",queued.input.message_id)),"编辑补充",Editor::QueuedEdit(queued.clone()),BTreeMap::from([("text",queued.input.text.clone())]),cx)));
            for file in &queued.input.attachments {
                page=page.child(self.record_attachment(&queued.input.message_id, conversation_id, file, cx));
            }
        }
        // The work RPC has no artifact list. Only display attachment references with an exact
        // source conversation, never manufacture file paths from the result's prose.
        if let Some(history) = self
            .snapshot
            .channel_history
            .as_ref()
            .filter(|h| h.network_id == detail.network_id)
        {
            for event in &history.events {
                if let HistoryBody::InputPosted { input } = &event.body {
                    if event.scope_id == *conversation_id
                        && work.target.as_ref() == Some(&input.target)
                    {
                        if !input.attachments.is_empty() {
                            page = page.child(self.row(
                                "来源任务中的附件",
                                format!(
                                    "输入 {} · {}",
                                    input.message_id,
                                    if history.coverage.truncated {
                                        "任务历史已截断"
                                    } else {
                                        "已读取当前任务范围"
                                    }
                                ),
                            ));
                        }
                        for file in &input.attachments {
                            page = page.child(self.record_attachment(
                                &event.event_hash,
                                conversation_id,
                                file,
                                cx,
                            ));
                        }
                    }
                }
            }
        }
        for question in self.snapshot.questions.iter().filter(|q| {
            q.conversation_id == *conversation_id
                && q.answer.is_none()
                && q.closed.is_none()
                && (matches!(&q.target,Some(InputTarget::Work{work_id:id}) if id == work_id)
                    || (active
                        && q.about.as_ref().is_some_and(|about| {
                            about.node == work.target_node
                                && Some(about.session_id.as_str()) == work.thread_id.as_deref()
                                && about.request_id.as_deref().is_some_and(|id| !id.is_empty())
                        })))
        }) {
            let mut row = self.row("工作需要你回复", question.text.clone());
            for choice in &question.choices {
                row = row.child(
                    self.button(
                        SharedString::from(format!(
                            "work-answer-{}-{}",
                            question.question_id, choice.label
                        )),
                        choice.label.clone(),
                        Command::Answer {
                            conversation_id: conversation_id.clone(),
                            question_id: question.question_id.clone(),
                            text: choice.label.clone(),
                        },
                        cx,
                    )
                    .disabled(!self.snapshot.writable),
                );
            }
            page = page.child(row.child(self.open_editor(
                SharedString::from(format!("work-answer-text-{}", question.question_id)),
                "输入回复",
                Editor::Answer(question.question_id.clone(), conversation_id.clone()),
                BTreeMap::new(),
                cx,
            )));
        }
        page.child(self.record_artifact_preview(cx))
            .child(self.record_composer(
                conversation_id.clone(),
                InputTarget::Work {
                    work_id: work_id.clone(),
                },
                None,
                active && !uncertain && work.state != WorkState::Interrupting,
                cx,
            ))
            .into_any_element()
    }

    fn nav_work_link(
        &self,
        label: &str,
        conversation_id: &str,
        work_id: &str,
        cx: &mut Context<Self>,
    ) -> Button {
        let route = Route::Work {
            conversation_id: conversation_id.into(),
            work_id: work_id.into(),
        };
        let generation = self.snapshot.generation;
        let network = self.snapshot.network.clone();
        Button::new(SharedString::from(format!("linked-{work_id}")))
            .label(label.to_owned())
            .on_click(cx.listener(move |this, _, _, cx| {
                if this.same_view_scope(generation, &network) {
                    this.navigate(route.clone(), cx)
                }
            }))
    }
}

fn work_state_label(state: WorkState) -> &'static str {
    match state {
        WorkState::Accepted => "已接收",
        WorkState::Running => "执行中",
        WorkState::Interrupting => "等待停止确认",
        WorkState::Finished => "已结束",
        WorkState::Unverified => "结果待核实",
    }
}
pub(crate) fn outcome_label(outcome: WorkOutcome) -> &'static str {
    match outcome {
        WorkOutcome::Completed => "已完成",
        WorkOutcome::Failed => "失败",
        WorkOutcome::Interrupted => "已停止",
        WorkOutcome::Rejected => "被拒绝",
        WorkOutcome::Unverified => "未核实",
    }
}
