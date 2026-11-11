use super::*;

impl Backend {
    pub(super) async fn save_workspace(
        &mut self,
        editor: Editor,
        values: BTreeMap<String, String>,
    ) -> Result<()> {
        #[cfg(test)]
        Self::test_phase("save-workspace-enter");
        let get = |key: &str| values.get(key).cloned().unwrap_or_default();
        let new_id = || {
            if get("_submission_id").is_empty() {
                uuid::Uuid::new_v4().to_string()
            } else {
                get("_submission_id")
            }
        };
        let csv = |key: &str| {
            get(key)
                .split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_owned)
                .collect::<Vec<_>>()
        };
        match editor {
            Editor::CreateTask => {
                self.submit(Action::Workspace {
                    command: WorkspaceCommand::CreateTask {
                        task_id: new_id(),
                        channel_id: None,
                        parent_task_id: None,
                        owner_bot: get("owner_bot"),
                        title: get("title"),
                        request: get("request"),
                        expected_result: get("expected_result"),
                        source_events: vec![],
                        dependencies: csv("dependencies"),
                        reply_only: false,
                    },
                })
                .await?
            }
            Editor::EditTask(task_id, expected_revision) => {
                self.submit(Action::Workspace {
                    command: WorkspaceCommand::ReviseTask {
                        task_id,
                        expected_revision,
                        request: get("request"),
                        expected_result: get("expected_result"),
                        dependencies: csv("dependencies"),
                    },
                })
                .await?
            }
            Editor::ChannelPost(channel_id) => {
                let attachments = self
                    .attachments
                    .get(&format!("channel-post:{channel_id}"))
                    .cloned()
                    .unwrap_or_default();
                self.submit(Action::Workspace {
                    command: WorkspaceCommand::PostChannelMessage {
                        channel_id,
                        message_id: new_id(),
                        text: get("text"),
                        attachments,
                        recipient_bot: (!get("recipient_bot").is_empty())
                            .then(|| get("recipient_bot")),
                        task_id: (!get("task_id").is_empty()).then(|| get("task_id")),
                        reply_to: (!get("reply_to").is_empty()).then(|| get("reply_to")),
                    },
                })
                .await?;
            }
            Editor::CreateChannel => {
                self.submit(Action::Workspace {
                    command: WorkspaceCommand::CreateChannel {
                        channel_id: new_id(),
                        title: get("title"),
                        purpose: get("purpose"),
                        members: csv("members"),
                        coordinator_bot: get("coordinator_bot"),
                        coordinator_node: get("coordinator_node"),
                    },
                })
                .await?
            }
            Editor::EditChannel(channel_id, expected_revision, owner_epoch) => {
                self.submit(Action::Workspace {
                    command: WorkspaceCommand::ConfigureChannel {
                        channel_id,
                        expected_revision,
                        owner_epoch,
                        title: get("title"),
                        purpose: get("purpose"),
                        members: csv("members"),
                        coordinator_bot: get("coordinator_bot"),
                        coordinator_node: get("coordinator_node"),
                    },
                })
                .await?
            }
            Editor::AssignTask(task_id, expected_revision) => {
                let bot_id = get("bot_id");
                let bot = self
                    .state
                    .bots
                    .iter()
                    .find(|b| b.bot_id == bot_id)
                    .context("执行Bot未加载")?;
                let bot_conversation = bot.conversation_id.clone();
                let conversation = self
                    .client()?
                    .read(ReadRequest::Conversation {
                        conversation_id: bot_conversation.clone(),
                    })
                    .await?;
                ensure!(
                    conversation["conversation"]["conversation_id"] == bot_conversation,
                    "Bot拥有者响应属于其他对话"
                );
                let bot_owner_epoch = conversation["conversation"]["owner_epoch"]
                    .as_u64()
                    .context("Bot拥有者代数未确认")?;
                let channel_owner_epoch = self
                    .state
                    .tasks
                    .iter()
                    .find(|task| task.task_id == task_id)
                    .and_then(|task| task.channel_id.as_ref())
                    .and_then(|id| self.state.channels.iter().find(|c| &c.channel_id == id))
                    .map(|c| c.owner_epoch);
                self.submit(Action::Workspace {
                    command: WorkspaceCommand::AssignTask {
                        task_id,
                        expected_revision,
                        assignment_id: new_id(),
                        bot_id,
                        execution_node: get("execution_node"),
                        bot_owner_epoch,
                        channel_owner_epoch,
                        work_id: None,
                        resource_roots: get("resource_roots").lines().map(str::to_owned).collect(),
                        automatic: get("automatic") == "true",
                    },
                })
                .await?;
            }
            Editor::QueuedEdit(queued) => {
                self.submit(Action::Workspace {
                    command: WorkspaceCommand::ControlInput {
                        message_id: queued.input.message_id,
                        expected_event: queued.control_event,
                        action: vibeface_protocol::workspace::InputControl::Edit {
                            text: get("text"),
                            attachments: queued.input.attachments,
                        },
                    },
                })
                .await?
            }
            Editor::Answer(question_id, conversation_id) => {
                self.submit(Action::Answer {
                    conversation_id,
                    question_id,
                    text: get("text"),
                })
                .await?
            }
            _ => unreachable!("editor routed to the wrong workspace handler"),
        }
        Ok(())
    }
}
