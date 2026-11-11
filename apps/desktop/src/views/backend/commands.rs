use super::*;
use std::{future::Future, pin::Pin};

impl Backend {
    pub(super) async fn run(&mut self, command: Command) -> Result<()> {
        #[cfg(test)]
        Self::test_phase("run-enter");
        self.dispatch_command(command).await
    }

    // The monolithic async match reserved about 1.6 MiB for debug poll temporaries.
    // Each command now owns one future behind this dispatch boundary.
    fn dispatch_command(
        &mut self,
        command: Command,
    ) -> Pin<Box<dyn Future<Output = Result<()>> + Send + '_>> {
        match command {
            Command::RestoreProfile {
                conversation_id,
                record,
            } => Box::pin(async move {
                self.restore_profile(&conversation_id, &record).await?;
                Ok(())
            }),
            Command::LoadHistory {
                conversation_id,
                section,
            } => Box::pin(async move {
                self.start_history_page(&conversation_id, section).await?;
                Ok(())
            }),
            Command::SendStaged(id) => Box::pin(async move {
                {
                    ensure!(self.state.online, "原连接尚未恢复，操作保留在本机");
                    let client = self.client()?.clone();
                    let completion = self.completion.clone();
                    let generation = client.generation().to_owned();
                    tokio::spawn(async move {
                        let result = client.send_staged(&id).await;
                        let _ = completion
                            .send(Completed {
                                generation,
                                input: None,
                                form_id: None,
                                copy_token: false,
                                delivery: false,
                                result,
                            })
                            .await;
                    });
                };
                Ok(())
            }),
            Command::ReviewSettings(id) => Box::pin(async move {
                self.review_settings(&id).await?;
                Ok(())
            }),
            Command::AcknowledgeOperatorUnknown(node_id) => Box::pin(async move {
                self.acknowledge_operator(&node_id).await?;
                Ok(())
            }),
            Command::SwitchProfile(id) => Box::pin(async move {
                self.switch_profile(&id).await?;
                Ok(())
            }),
            Command::WatchRecord(record) => Box::pin(async move {
                {
                    self.active_record = record;
                    self.last_record_read = tokio::time::Instant::now();
                };
                Ok(())
            }),
            Command::ConfirmIdentityBackup(digest) => Box::pin(async move {
                {
                    let (key, plan) = self
                        .identity_plan
                        .as_mut()
                        .context("没有待确认的恢复文件")?;
                    ensure!(
                        plan.backup_sha256() == digest,
                        "恢复文件确认与当前计划不一致"
                    );
                    plan.submit(key).await?;
                    self.publish_identity_plan();
                    let _ = self
                        .emit(Event::Notice(
                            "已提交原身份计划。等待恢复生效或管理设备处理，未自动恢复写入。".into(),
                        ))
                        .await;
                };
                Ok(())
            }),
            Command::ReconcileIdentity => Box::pin(async move {
                {
                    let (key, plan) = self
                        .identity_plan
                        .as_mut()
                        .context("请先解锁并读取原身份计划")?;
                    plan.reconcile(key).await?;
                    self.publish_identity_plan();
                };
                Ok(())
            }),
            Command::DiscardIdentity => Box::pin(async move {
                {
                    let Some((key, plan)) = self.identity_plan.take() else {
                        anyhow::bail!("没有已读取的身份计划")
                    };
                    if !matches!(
                        plan.state(),
                        vibeface_client::IdentityOperationState::Prepared
                            | vibeface_client::IdentityOperationState::Applied
                            | vibeface_client::IdentityOperationState::Stopped
                    ) {
                        self.identity_plan = Some((key, plan));
                        anyhow::bail!("身份计划结果未知，须先对账");
                    }
                    plan.discard()?;
                    self.state.identity_plan = None;
                    self.publish();
                };
                Ok(())
            }),
            Command::Refresh => Box::pin(async move {
                if self.client.is_some() {
                    if self.state.online {
                        self.refresh().await?
                    } else {
                        self.retry_at = tokio::time::Instant::now();
                        self.maintain().await?;
                    }
                }
                Ok(())
            }),
            Command::Disconnect => Box::pin(async move {
                {
                    self.cancel_reconnect().await?;
                    if let Some(client) = self.client.take() {
                        client.close();
                    }
                    self.active_record = ActiveRecord::None;
                    self.pushes = None;
                    self.reconnect = None;
                    self.active_entry = None;
                    self.conversation = None;
                    self.attachments.clear();
                    self.key = None;
                    self.state = Snapshot {
                        generation: self.state.generation + 1,
                        status: "已断开。个人网络数据和 Node 保持原样。".into(),
                        ..Default::default()
                    };
                    self.publish();
                };
                Ok(())
            }),
            Command::Conversation(id) => Box::pin(async move {
                {
                    ensure!(
                        self.state.bots.iter().any(|bot| bot.conversation_id == id),
                        "当前档案没有已验证的Bot目标"
                    );
                    if self.conversation.as_ref() != Some(&id) {
                        self.history_state = None;
                    }
                    self.state.live.clear();
                    self.conversation = Some(id);
                    if self.state.online {
                        self.refresh().await?;
                    } else {
                        self.publish();
                    }
                };
                Ok(())
            }),
            Command::Save {
                editor,
                values,
                conversation_id,
            } => Box::pin(async move {
                {
                    let id = values.get("_submission_id").cloned();
                    self.form_id = id.clone();
                    self.form_owner = values.get("_owner_node_id").cloned();
                    let immediate = matches!(
                        &editor,
                        Editor::Connect
                            | Editor::ConnectLocal
                            | Editor::CreateNetwork
                            | Editor::Join
                            | Editor::Recovery
                            | Editor::CreateProfile
                            | Editor::CloseSettings(_)
                            | Editor::Logs
                            | Editor::Member(_)
                    );
                    let identity_operation = matches!(
                        &editor,
                        Editor::Connect
                            | Editor::ConnectLocal
                            | Editor::Join
                            | Editor::CreateNetwork
                            | Editor::Recovery
                    );
                    if identity_operation {
                        self.identity_busy = true;
                        self.publish();
                    }
                    let result = self.save(editor, values, conversation_id).await;
                    if identity_operation {
                        self.identity_busy = false;
                        self.publish();
                    }
                    self.form_id = None;
                    self.form_owner = None;
                    result?;
                    if immediate && let Some(id) = id {
                        let _ = self.emit(Event::FormSaved(id)).await;
                    }
                };
                Ok(())
            }),
            Command::Input {
                conversation_id,
                draft,
                owner,
            } => Box::pin(async move {
                let result = async {
                    draft.validate().map_err(anyhow::Error::msg)?;
                    self.send_draft(conversation_id, draft, None, owner.clone())
                        .await
                }
                .await;
                if let Err(error) = result {
                    if let Some(owner) = owner {
                        let _ = self
                            .emit(Event::InputFailed {
                                owner,
                                error: format!("输入保存或确认未完成，编辑器内容保留：{error}"),
                            })
                            .await;
                    } else {
                        return Err(error);
                    }
                }
                Ok(())
            }),
            Command::RetryReply {
                conversation_id,
                message_id,
            } => Box::pin(async move {
                {
                    ensure!(
                        self.conversation.as_ref() == Some(&conversation_id),
                        "请先打开原对话再重试"
                    );
                    let Some(shown) = self
                        .retry_reply_proof(&conversation_id, &message_id)
                        .await?
                    else {
                        return Ok(());
                    };
                    let asked = shown
                        .messages
                        .iter()
                        .find(|message| message.message_id == message_id && message.role == "user")
                        .context("原输入尚未读取")?
                        .clone();
                    let newest = shown.messages.last().context("对话没有消息")?;
                    ensure!(
                        newest.message_id == message_id
                            || newest.in_reply_to.as_ref() == Some(&message_id),
                        "原输入已有新进展，没有重发"
                    );
                    let job = shown
                        .scheduler
                        .retry_job_for_message(&message_id)
                        .context("拥有者不可达或尚未读取原回复的当前执行状态，不能安全重试")?;
                    ensure!(
                        job.can_retry(),
                        "原回复尚未证明可安全重试，请先核对执行记录"
                    );
                    let bot_id = self
                        .state
                        .bots
                        .iter()
                        .find(|bot| bot.conversation_id == conversation_id)
                        .context("原Bot尚未读取")?
                        .bot_id
                        .clone();
                    let client = self.client()?.clone();
                    let mut attachments = vec![];
                    for file in &asked.attachments {
                        let bytes = fetch_attachment(&client, &conversation_id, file).await?;
                        attachments.push(client.store_attachment(file.name.clone(), bytes).await?);
                    }
                    let text = format!(
                        "上次没回完，可能已经做了一部分。先看看做过什么，做过的不要重做，再接着完成：\n{}",
                        asked.text
                    );
                    self.send_draft(
                        conversation_id,
                        ComposerDraft {
                            target: InputTarget::BotMain { bot_id },
                            intent: InputIntent::Continue,
                            text,
                            attachments,
                            expected_revision: None,
                        },
                        None,
                        None,
                    )
                    .await?;
                };
                Ok(())
            }),
            Command::ResumeDraft(fingerprint) => Box::pin(async move {
                {
                    let path = self.draft_dir()?.join(format!("{fingerprint}.json"));
                    let saved = read_draft(path).await?;
                    self.send_draft(
                        saved.conversation_id.clone(),
                        ComposerDraft {
                            target: saved.input.route.target.clone(),
                            intent: saved.input.route.intent,
                            text: saved.input.text.clone(),
                            attachments: saved.input.attachments.clone(),
                            expected_revision: saved.input.route.expected_revision,
                        },
                        Some(saved),
                        None,
                    )
                    .await?;
                };
                Ok(())
            }),
            Command::Workspace(command) => Box::pin(async move {
                self.submit(Action::Workspace { command }).await?;
                Ok(())
            }),
            Command::StopWork {
                conversation_id,
                work_id,
            } => Box::pin(async move {
                self.submit(Action::StopWork {
                    conversation_id,
                    work_id,
                })
                .await?;
                Ok(())
            }),
            Command::StopReply(conversation_id) => Box::pin(async move {
                {
                    ensure!(
                        self.conversation.as_deref() == Some(&conversation_id),
                        "请先打开原对话再停止回复"
                    );
                    let triggers = self
                        .state
                        .conversation
                        .scheduler
                        .effective_jobs()
                        .into_iter()
                        .filter(|job| job.is_active())
                        .map(|j| j.trigger.clone())
                        .collect::<Vec<_>>();
                    ensure!(!triggers.is_empty(), "没有已确认的活动回复可停止");
                    for trigger in triggers {
                        self.submit(Action::StopReply {
                            conversation_id: conversation_id.clone(),
                            trigger,
                        })
                        .await?;
                    }
                };
                Ok(())
            }),
            Command::Answer {
                conversation_id,
                question_id,
                text,
                ..
            } => Box::pin(async move {
                self.submit(Action::Answer {
                    conversation_id,
                    question_id,
                    text,
                })
                .await?;
                Ok(())
            }),
            Command::Reconcile(id) => Box::pin(async move {
                {
                    let operation = self
                        .client()?
                        .pending()
                        .await?
                        .into_iter()
                        .find(|operation| operation.id == id)
                        .context("原操作已经结算或不属于当前档案")?;
                    ensure!(
                        operation.state == vibeface_client::OperationState::Unknown,
                        "此处只查询未知结果；尚未发送的操作须明确选择发送"
                    );
                    let result = self.client()?.reconcile(&id).await?;
                    if result.state != vibeface_client::OperationState::Unknown
                        && self
                            .state
                            .settings_review
                            .as_ref()
                            .is_some_and(|view| view.review.operation_id == id)
                    {
                        self.state.settings_review = None;
                    }
                    let _ = self
                        .emit(Event::Notice(format!(
                            "操作回执：{}",
                            operation_label(result.state)
                        )))
                        .await;
                    if result.state == vibeface_client::OperationState::Accepted {
                        self.finish_accepted_draft(&result.id).await?;
                    }
                    self.refresh().await?;
                    self.flush_prepared().await?;
                };
                Ok(())
            }),
            Command::DeleteBot(conversation_id) => Box::pin(async move {
                self.submit(Action::DeleteBot { conversation_id }).await?;
                Ok(())
            }),
            Command::PrimaryBot(conversation_id) => Box::pin(async move {
                self.submit(Action::SetPrimaryBot { conversation_id })
                    .await?;
                Ok(())
            }),
            Command::DeleteMemory {
                conversation_id,
                memory,
            } => Box::pin(async move {
                self.submit(Action::Memory {
                    conversation_id,
                    memory_id: memory.memory_id,
                    base: memory_base(&memory.metadata)?,
                    after: None,
                })
                .await?;
                Ok(())
            }),
            Command::Schedule {
                conversation_id,
                schedule_id,
                action,
            } => Box::pin(async move {
                {
                    let action = match action.as_str() {
                        "pause" => ScheduleAction::Pause,
                        "resume" => ScheduleAction::Resume,
                        "delete" => ScheduleAction::Delete,
                        _ => anyhow::bail!("未知计划动作"),
                    };
                    self.submit(Action::Schedule {
                        conversation_id,
                        schedule_id,
                        action,
                    })
                    .await?;
                };
                Ok(())
            }),
            Command::Identity(action) => Box::pin(async move {
                {
                    let refresh = !matches!(
                        &action,
                        IdentityAction::Directory | IdentityAction::Requests
                    );
                    self.identity(action).await?;
                    if refresh {
                        self.identity(IdentityAction::Directory).await?;
                        self.identity(IdentityAction::Requests).await?;
                    }
                };
                Ok(())
            }),
            Command::Host(action) => Box::pin(async move {
                {
                    match action {
                        HostAction::Refresh => {
                            self.host.discover().await?;
                        }
                        HostAction::Controller => self.host.choose_mode(NodeMode::Controller)?,
                        HostAction::Select(identity) => {
                            self.host.select_node(&identity).await?;
                        }
                        HostAction::Start => {
                            self.host.start_owned().await?;
                        }
                        HostAction::Stop => self.host.stop_owned().await?,
                        HostAction::Background(action) => {
                            self.host.background(action).await?;
                        }
                        HostAction::Remember(path) => {
                            self.host.remember_directory(path).await?;
                            self.host.discover().await?;
                        }
                        HostAction::StartCandidate(node) => {
                            self.host.start_candidate(&node).await?;
                        }
                        HostAction::ClearSelection => self.host.clear_selection()?,
                        HostAction::Preferences(preferences) => {
                            self.host.save_preferences(preferences)?
                        }
                    }
                    self.publish_host().await;
                };
                Ok(())
            }),
            Command::WorkDetail {
                conversation_id,
                work_id,
            } => Box::pin(async move {
                self.read_work_detail(conversation_id, work_id).await?;
                Ok(())
            }),
            Command::ResolveWork {
                conversation_id,
                work_id,
                outcome,
            } => Box::pin(async move {
                self.submit(Action::ResolveCheck {
                    conversation_id,
                    check_id: format!("work:{work_id}"),
                    work_id: Some(work_id),
                    outcome: Some(outcome),
                    note: String::new(),
                })
                .await?;
                Ok(())
            }),
            Command::Ack(note) => Box::pin(async move {
                self.submit(Action::AckNotification {
                    conversation_id: note.conversation_id,
                    notification_id: note.notification_id,
                })
                .await?;
                Ok(())
            }),
            Command::ResolveCheck(check) => Box::pin(async move {
                self.submit(Action::ResolveCheck {
                    conversation_id: check.conversation_id,
                    check_id: check.check_id,
                    work_id: None,
                    outcome: None,
                    note: String::new(),
                })
                .await?;
                Ok(())
            }),
            Command::ConfirmWritable => Box::pin(async move {
                self.submit(Action::ConfirmWritable).await?;
                Ok(())
            }),
            Command::History {
                channel_id,
                task_id,
            } => Box::pin(async move {
                self.read_history(channel_id, task_id).await?;
                Ok(())
            }),
            Command::EditSettings(kind) => Box::pin(async move {
                self.edit_settings(kind, false).await?;
                Ok(())
            }),
            Command::EditOwnerSettings(kind) => Box::pin(async move {
                self.edit_settings(kind, true).await?;
                Ok(())
            }),
            Command::ProviderRead { provider, probe } => Box::pin(async move {
                {
                    let value = self
                        .read_node(if probe {
                            ReadRequest::ProviderProbe { provider }
                        } else {
                            ReadRequest::ProviderModels { provider }
                        })
                        .await?;
                    let mut rows = vec![];
                    if let Some(models) = value["models"].as_array() {
                        for model in models {
                            rows.push((
                                model["name"]
                                    .as_str()
                                    .or_else(|| model["id"].as_str())
                                    .unwrap_or("模型")
                                    .into(),
                                model["id"].as_str().unwrap_or("未提供模型 ID").into(),
                            ));
                        }
                    }
                    rows.push((
                        "可用性".into(),
                        value["state"]
                            .as_str()
                            .or_else(|| value["readiness"]["state"].as_str())
                            .unwrap_or("未确认")
                            .into(),
                    ));
                    let _ = self.emit(Event::Detail(rows)).await;
                };
                Ok(())
            }),
            Command::ReadTriage => Box::pin(async move {
                {
                    let value = self.read_node(ReadRequest::TriageSettings).await?;
                    self.state.decision_tokens = decode_list(&value, "decision_tokens")?;
                    self.publish();
                };
                Ok(())
            }),
            Command::TestWebhook => Box::pin(async move {
                self.submit(Action::TestWebhook).await?;
                Ok(())
            }),
            Command::DecisionTokenRevoke(token_id) => Box::pin(async move {
                self.submit(Action::RevokeDecisionToken { token_id })
                    .await?;
                Ok(())
            }),
            Command::ArtifactPreview {
                conversation_id,
                file,
                offset,
            } => Box::pin(async move {
                {
                    let page = self
                        .client()?
                        .read(ReadRequest::Artifact {
                            conversation_id: conversation_id.clone(),
                            blob_hash: file.hash.clone(),
                            offset,
                            limit: 65536,
                        })
                        .await?;
                    let network = self.client()?.network_id().to_owned();
                    let page = crate::views::records::parse_artifact_page(
                        page,
                        &network,
                        &conversation_id,
                        &file,
                        offset,
                    )?;
                    self.state.artifact_preview = Some(Arc::new(page));
                    self.publish();
                };
                Ok(())
            }),
            Command::ArtifactSave {
                conversation_id,
                file,
                path,
            } => Box::pin(async move {
                {
                    ensure!(file.size <= 64 * 1024 * 1024, "附件超过64MiB");
                    let client = self.client()?.clone();
                    let mut bytes = Vec::with_capacity(file.size as usize);
                    let mut offset = 0;
                    loop {
                        let page = client
                            .read(ReadRequest::ArtifactBytes {
                                conversation_id: conversation_id.clone(),
                                blob_hash: file.hash.clone(),
                                offset,
                            })
                            .await?;
                        ensure!(
                            page["blob_hash"] == file.hash
                                && page["size"].as_u64() == Some(file.size)
                                && page["offset"].as_u64() == Some(offset),
                            "附件分块响应与原引用不一致"
                        );
                        let part = base64::engine::general_purpose::STANDARD
                            .decode(page["data"].as_str().context("附件分块缺少内容")?)?;
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
                            "附件游标未按实际字节推进"
                        );
                    }
                    ensure!(
                        bytes.len() as u64 == file.size
                            && format!("{:x}", Sha256::digest(&bytes)) == file.hash,
                        "附件SHA-256不一致，未保存"
                    );
                    crate::host::platform::save_selected(Some(path), &bytes)?;
                    let _ = self.emit(Event::Notice("附件已保存。".into())).await;
                };
                Ok(())
            }),
            Command::DiagnosticsSave(path) => Box::pin(async move {
                {
                    let value = self.read_node(ReadRequest::Diagnostics).await?;
                    let bytes = serde_json::to_vec_pretty(&value)?;
                    crate::host::platform::save_selected(Some(path), &bytes)?;
                    let _ = self.emit(Event::Notice("脱敏诊断包已保存。".into())).await;
                };
                Ok(())
            }),
            Command::ReadMemory {
                conversation_id,
                memory_id,
            } => Box::pin(async move {
                {
                    let value = self
                        .client()?
                        .read(ReadRequest::Memory {
                            conversation_id,
                            memory_id: memory_id.clone(),
                        })
                        .await?;
                    ensure!(
                        value["current"].is_null() || value["current"]["memory_id"] == memory_id,
                        "记忆响应属于其他条目"
                    );
                    let current = &value["current"];
                    let rows = vec![
                        (
                            "当前记忆".into(),
                            current["text"]
                                .as_str()
                                .unwrap_or("已删除或此处未同步")
                                .into(),
                        ),
                        (
                            "当前版本".into(),
                            current["metadata"]["revision"]
                                .as_str()
                                .unwrap_or("未知")
                                .into(),
                        ),
                        (
                            "回执".into(),
                            value["receipt"].as_str().unwrap_or("未查询操作").into(),
                        ),
                    ];
                    let _ = self.emit(Event::Detail(rows)).await;
                };
                Ok(())
            }),
            Command::ScopeContext {
                conversation_id,
                target,
            } => Box::pin(async move {
                {
                    let value = self
                        .client()?
                        .read(ReadRequest::Context {
                            conversation_id: conversation_id.clone(),
                            target: target.clone(),
                        })
                        .await?;
                    ensure!(
                        value["conversation_id"] == conversation_id
                            && value["target"] == serde_json::to_value(&target)?,
                        "上下文属于其他对话或输入目标"
                    );
                    let context = &value["context"];
                    if value["available"] == false {
                        let _ = self
                            .emit(Event::Detail(vec![(
                                "上下文暂不可读".into(),
                                value["reason"].as_str().unwrap_or("设备未提供原因").into(),
                            )]))
                            .await;
                    } else {
                        let scope = &context["scope"];
                        let matches = match &target {
                            InputTarget::BotMain { .. } => scope["kind"] == "main",
                            InputTarget::Task { task_id }
                            | InputTarget::ChannelTask { task_id, .. } => {
                                scope["kind"] == "workspace_task" && scope["task_id"] == *task_id
                            }
                            InputTarget::Work { work_id } => {
                                scope["kind"] == "task" && scope["work_id"] == *work_id
                            }
                            InputTarget::Routine { trigger, .. } => {
                                scope["kind"] == "routine" && scope["trigger"] == *trigger
                            }
                            InputTarget::ChannelMember { .. } => false,
                        };
                        ensure!(matches, "上下文范围与输入目标不一致");
                        let mut rows = vec![
                            (
                                "Provider".into(),
                                context["provider"].as_str().unwrap_or("未知").into(),
                            ),
                            (
                                "上下文阶段".into(),
                                context["stage"].as_str().unwrap_or("未知").into(),
                            ),
                            (
                                "历史检索范围".into(),
                                context["coverage"]["history_retrieval"]
                                    .as_str()
                                    .unwrap_or("未提供")
                                    .into(),
                            ),
                        ];
                        if let Some(sources) = context["sources"].as_array() {
                            for source in sources {
                                rows.push((
                                    source["kind"].as_str().unwrap_or("来源").into(),
                                    source["reference"]
                                        .as_str()
                                        .or_else(|| source["event_id"].as_str())
                                        .unwrap_or("引用未提供")
                                        .into(),
                                ));
                            }
                        }
                        rows.push((
                            "省略来源".into(),
                            context["sources_omitted"].as_u64().unwrap_or(0).to_string(),
                        ));
                        let _ = self.emit(Event::Detail(rows)).await;
                    }
                };
                Ok(())
            }),
            Command::NodeSettings(target) => Box::pin(async move {
                {
                    ensure!(
                        self.active_entry
                            .as_ref()
                            .is_some_and(|entry| entry.node_id == target)
                            || self.client.is_none()
                                && self
                                    .host
                                    .snapshot()
                                    .selected
                                    .as_ref()
                                    .is_some_and(|node| node.device_id == target),
                        "请先连接所选设备，再读取或修改其配置"
                    );
                    let value = self.read_node(ReadRequest::NodeSettings).await?;
                    let _ = self.emit(Event::Detail(settings_rows(&value))).await;
                };
                Ok(())
            }),
            Command::ReadLogs { search } => Box::pin(async move {
                {
                    let value = self.read_node(ReadRequest::Logs).await?;
                    let rows = value["records"]
                        .as_array()
                        .map(|items| {
                            items
                                .iter()
                                .filter_map(|entry| {
                                    let message = entry["message"].as_str()?;
                                    if !search.is_empty() && !message.contains(&search) {
                                        return None;
                                    }
                                    Some((
                                        entry["level"].as_str().unwrap_or("info").into(),
                                        message.into(),
                                    ))
                                })
                                .collect()
                        })
                        .unwrap_or_default();
                    let _ = self.emit(Event::Detail(rows)).await;
                };
                Ok(())
            }),
            Command::Transfers => Box::pin(async move {
                self.refresh_delivery().await?;
                Ok(())
            }),
            Command::DeliveryConfigure {
                enabled,
                charity,
                paid,
            } => Box::pin(async move {
                self.submit(Action::Delivery(
                    vibeface_client::DeliveryAction::Configure {
                        enabled,
                        charity_enabled: charity,
                        paid_enabled: paid,
                    },
                ))
                .await?;
                Ok(())
            }),
            Command::DeliveryConfirm(order) => Box::pin(async move {
                {
                    ensure!(
                        order.phase == "prepared" && order.role == "sender",
                        "先查询原订单，当前订单不可确认"
                    );
                    let current = self
                        .client()?
                        .read(ReadRequest::Delivery(DeliveryRead::Get {
                            order_id: order.order_id.clone(),
                        }))
                        .await?;
                    let current: TransferOrder = serde_json::from_value(current)?;
                    validate_transfer_order(&current)?;
                    self.validate_quote(&current).await?;
                    ensure!(same_terms(&order, &current), "订单条款已变化，未确认付款");
                    self.submit(Action::Delivery(vibeface_client::DeliveryAction::Confirm {
                        intent_id: order.intent_id,
                    }))
                    .await?;
                };
                Ok(())
            }),
            Command::DeliveryAccept(order_id) => Box::pin(async move {
                self.submit(Action::Delivery(vibeface_client::DeliveryAction::Accept {
                    order_id,
                }))
                .await?;
                Ok(())
            }),
            Command::DeliveryReconcile(order_id) => Box::pin(async move {
                self.submit(Action::Delivery(
                    vibeface_client::DeliveryAction::Reconcile { order_id },
                ))
                .await?;
                Ok(())
            }),
            Command::DeliverySave { order, path } => Box::pin(async move {
                {
                    ensure!(can_read_transfer(&order), "原订单文件尚不可读取");
                    let value = self
                        .client()?
                        .read(ReadRequest::Delivery(DeliveryRead::Read {
                            order_id: order.order_id.clone(),
                        }))
                        .await?;
                    let encoded = value["content_base64"]
                        .as_str()
                        .context("文件响应缺少内容")?;
                    let bytes = base64::engine::general_purpose::STANDARD.decode(encoded)?;
                    ensure!(
                        value["name"].as_str() == order.name.as_deref()
                            && value["bytes"].as_u64() == Some(order.bytes)
                            && value["sha256"].as_str() == order.sha256.as_deref(),
                        "文件信息与订单不匹配，未保存"
                    );
                    ensure!(
                        bytes.len() as u64 == order.bytes
                            && format!("{:x}", Sha256::digest(&bytes))
                                == order.sha256.as_deref().unwrap_or(""),
                        "文件大小或 SHA-256 不匹配，未保存"
                    );
                    match crate::host::platform::save_selected(Some(path), &bytes)? {
                        crate::host::platform::SaveResult::Saved(_) => {
                            let _ = self.emit(Event::Notice("文件已保存。".into())).await;
                        }
                        crate::host::platform::SaveResult::Cancelled => {}
                    }
                };
                Ok(())
            }),
            Command::SelectAttachmentScope(conversation_id) => Box::pin(async move {
                {
                    let attachments = self
                        .attachments
                        .get(&conversation_id)
                        .cloned()
                        .unwrap_or_default();
                    let _ = self
                        .emit(Event::Attachments {
                            conversation_id,
                            attachments,
                        })
                        .await;
                };
                Ok(())
            }),
            Command::ReleaseAttachment {
                conversation_id,
                hash,
            } => Box::pin(async move {
                {
                    if let Some(files) = self.attachments.get_mut(&conversation_id) {
                        files.retain(|file| file.hash != hash);
                    }
                    let attachments = self
                        .attachments
                        .get(&conversation_id)
                        .cloned()
                        .unwrap_or_default();
                    self.collect_current_attachments().await?;
                    let _ = self
                        .emit(Event::Attachments {
                            conversation_id,
                            attachments,
                        })
                        .await;
                };
                Ok(())
            }),
            Command::Attachments {
                conversation_id,
                paths,
            } => Box::pin(async move {
                {
                    ensure!(
                        self.state.can_stage_input(),
                        "当前档案没有可验证输入目标，未读取附件"
                    );
                    let client = self.client()?.clone();
                    let mut attachments = vec![];
                    for path in paths {
                        let name = path
                            .file_name()
                            .context("附件文件名无效")?
                            .to_string_lossy()
                            .into_owned();
                        let metadata = tokio::fs::metadata(&path).await?;
                        ensure!(metadata.len() <= 64 * 1024 * 1024, "附件超过 64 MiB");
                        let bytes = tokio::fs::read(&path).await?;
                        attachments.push(client.store_attachment(name, bytes).await?);
                    }
                    let files = self.attachments.entry(conversation_id.clone()).or_default();
                    for attachment in attachments {
                        if !files.iter().any(|file| {
                            file.hash == attachment.hash && file.name == attachment.name
                        }) {
                            files.push(attachment);
                        }
                    }
                    let attachments = files.clone();
                    self.collect_current_attachments().await?;
                    let _ = self
                        .emit(Event::Attachments {
                            conversation_id,
                            attachments,
                        })
                        .await;
                };
                Ok(())
            }),
            Command::Scoped { .. } => Box::pin(async move { anyhow::bail!("操作封装无效") }),
        }
    }
}
