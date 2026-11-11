//! Bounded read workers. Only this actor applies their results; writes keep their own outbox.
use super::*;
use crate::views::history::{ApplyResult, PageTicket, SnapshotTicket};

#[derive(Clone, PartialEq, Eq)]
struct ReadScope {
    profile: String,
    generation: u64,
    connection: String,
    network: String,
    client: String,
    conversation: Option<String>,
}
#[derive(Clone, PartialEq, Eq)]
enum RecordKey {
    None,
    Work(String, String),
    Channel(String),
    Task(String),
}
impl From<&ActiveRecord> for RecordKey {
    fn from(record: &ActiveRecord) -> Self {
        match record {
            ActiveRecord::None => Self::None,
            ActiveRecord::Work {
                conversation_id,
                work_id,
            } => Self::Work(conversation_id.clone(), work_id.clone()),
            ActiveRecord::Channel(id) => Self::Channel(id.clone()),
            ActiveRecord::Task(id) => Self::Task(id.clone()),
        }
    }
}
pub(super) struct PendingRead {
    id: String,
    scope: ReadScope,
    record: Option<RecordKey>,
    query: Option<RecordQuery>,
    client: Client,
    joined: bool,
    task: tokio::task::JoinHandle<()>,
}
#[derive(Default)]
pub(super) struct ReadTasks {
    refresh: Option<PendingRead>,
    page: Option<PendingRead>,
    record: Option<PendingRead>,
    record_again: bool,
    delivery: Option<PendingRead>,
    delivery_again: bool,
    refresh_again: bool,
    retry: Option<RetryAfterRefresh>,
    ready_retry: Option<(RetryAfterRefresh, Arc<Conversation>)>,
}
struct RetryAfterRefresh {
    scope: ReadScope,
    refresh_id: String,
    conversation: String,
    message: String,
}
pub(super) struct ReadCompleted {
    id: String,
    scope: ReadScope,
    result: ReadReply,
}
enum ReadReply {
    Refresh(Result<Box<RefreshReply>>),
    Page {
        ticket: PageTicket,
        conversation: String,
        result: Result<Value>,
    },
    Record {
        query: RecordQuery,
        result: Result<RecordReply>,
    },
    Delivery(Result<DeliveryReply>),
}
struct DeliveryReply {
    status: Value,
    orders: Value,
    providers: Value,
    budgets: Option<Result<Value>>,
}
struct RefreshReply {
    home: Value,
    nodes: Value,
    workspace: Value,
    write_state: Value,
    questions: Value,
    notifications: Value,
    conversation: Option<(SnapshotTicket, Value)>,
}
#[derive(Clone, PartialEq, Eq)]
pub(super) enum RecordQuery {
    Work {
        conversation: String,
        work: String,
    },
    History {
        channel: Option<String>,
        task: Option<String>,
    },
}
struct RecordReply {
    work: Option<Value>,
    history: Option<Result<Value>>,
}

pub(super) const READ_DRAIN_TIMEOUT: Duration = Duration::from_secs(5);

async fn finish_read_task(slot: &mut Option<PendingRead>) -> Result<()> {
    let Some(pending) = slot.as_mut() else {
        return Ok(());
    };
    if !pending.joined {
        let _ = tokio::time::timeout(READ_DRAIN_TIMEOUT, &mut pending.task)
            .await
            .context("读取任务已返回结果，但尚未确认完成清理。")?;
        pending.joined = true;
    }
    // This worker awaited its own storage before sending its result. The active Client
    // still owns this outbox, so unrelated cache jobs need draining only at handoff.
    slot.take();
    Ok(())
}

async fn stop_read(slot: &mut Option<PendingRead>) -> Result<()> {
    let Some(pending) = slot.as_mut() else {
        return Ok(());
    };
    if !pending.joined {
        pending.task.abort();
        let _ = tokio::time::timeout(READ_DRAIN_TIMEOUT, &mut pending.task)
            .await
            .context("无法确认读取任务已退出，原存储租约保持占用。")?;
        pending.joined = true;
    }
    // A canceled WriteState future can leave its already-started cache job running.
    // Keep the exact old Client alive until its read-only local storage work has drained.
    tokio::time::timeout(READ_DRAIN_TIMEOUT, pending.client.wait_for_read_storage())
        .await
        .context("读取缓存尚未写完，不能交接原存储租约。")?;
    slot.take();
    Ok(())
}

impl Backend {
    fn read_scope(&self) -> Result<ReadScope> {
        let client = self.client()?;
        Ok(ReadScope {
            profile: self.current_profile_id()?,
            generation: self.state.generation,
            connection: client.generation().into(),
            network: client.network_id().into(),
            client: client.client_id().into(),
            conversation: self.conversation.clone(),
        })
    }
    fn read_is_current(&self, pending: &PendingRead) -> bool {
        self.client.as_ref().is_some_and(Client::is_online)
            && self.read_scope().is_ok_and(|scope| scope == pending.scope)
            && pending
                .record
                .as_ref()
                .is_none_or(|record| record == &RecordKey::from(&self.active_record))
    }
    pub(super) async fn cancel_reads(&mut self) -> Result<()> {
        for pending in [
            &mut self.reads.refresh,
            &mut self.reads.page,
            &mut self.reads.record,
            &mut self.reads.delivery,
        ] {
            stop_read(pending).await?;
        }
        self.reads.refresh_again = false;
        self.reads.delivery_again = false;
        self.reads.record_again = false;
        if self.reads.retry.take().is_some() {
            let _ = self.out.try_send(Event::Scoped {
                generation: self.state.generation,
                profile_id: self.current_profile_id().ok(),
                event: Box::new(Event::Notice(
                    "读取范围已切换，原回复重试没有提交。请在原对话重新核对。".into(),
                )),
            });
        }
        self.reads.ready_retry = None;
        self.state.history_loading = None;
        while self.read_completions.try_recv().is_ok() {}
        Ok(())
    }
    pub(super) async fn cancel_stale_reads(&mut self) -> Result<()> {
        let stale = |pending: &Option<PendingRead>| {
            pending
                .as_ref()
                .is_some_and(|pending| !self.read_is_current(pending))
        };
        let refresh = stale(&self.reads.refresh);
        let page = stale(&self.reads.page);
        let record = stale(&self.reads.record);
        let delivery = stale(&self.reads.delivery);
        if refresh {
            stop_read(&mut self.reads.refresh).await?;
            self.reads.refresh_again = false;
            if self.reads.retry.take().is_some() {
                let _ = self.out.try_send(Event::Scoped {
                    generation: self.state.generation,
                    profile_id: self.current_profile_id().ok(),
                    event: Box::new(Event::Notice("连接已改变，原回复重试没有提交。".into())),
                });
            }
            self.reads.ready_retry = None;
        }
        if page {
            stop_read(&mut self.reads.page).await?;
            self.state.history_loading = None;
            self.publish();
        }
        if record {
            stop_read(&mut self.reads.record).await?;
            self.reads.record_again = false;
        }
        if delivery {
            stop_read(&mut self.reads.delivery).await?;
            self.reads.delivery_again = false;
        }
        Ok(())
    }
    pub(super) async fn prepare_read_scope_command(&mut self, command: &Command) -> Result<()> {
        let switching = matches!(
            command,
            Command::Disconnect | Command::SwitchProfile(_) | Command::Conversation(_)
        ) || matches!(command, Command::Save { editor, .. } if matches!(editor, Editor::Connect | Editor::ConnectLocal | Editor::Join | Editor::CreateNetwork | Editor::Recovery | Editor::CreateProfile));
        if switching {
            self.cancel_reads().await?;
        }
        Ok(())
    }
    pub(super) async fn retry_reply_proof(
        &mut self,
        conversation: &str,
        message: &str,
    ) -> Result<Option<Arc<Conversation>>> {
        let scope = self.read_scope()?;
        if self.reads.ready_retry.as_ref().is_some_and(|(ready, _)| {
            ready.scope == scope && ready.conversation == conversation && ready.message == message
        }) {
            return Ok(self.reads.ready_retry.take().map(|(_, proof)| proof));
        }
        ensure!(
            self.reads.retry.is_none(),
            "已有回复重试正在核对最新状态，请等待原请求完成。"
        );
        // This user intent needs a fresh batch started after the click, not an earlier batch.
        stop_read(&mut self.reads.refresh).await?;
        self.reads.refresh_again = false;
        self.start_refresh().await?;
        self.reads.retry = Some(RetryAfterRefresh {
            scope,
            refresh_id: self
                .reads
                .refresh
                .as_ref()
                .context("重试核对尚未开始")?
                .id
                .clone(),
            conversation: conversation.into(),
            message: message.into(),
        });
        Ok(None)
    }
    pub(super) fn refresh_in_flight(&self) -> bool {
        self.reads
            .refresh
            .as_ref()
            .is_some_and(|pending| self.read_is_current(pending))
    }
    pub(super) async fn start_refresh(&mut self) -> Result<()> {
        self.cancel_stale_reads().await?;
        if self.reads.refresh.is_some() {
            self.reads.refresh_again = true;
            return Ok(());
        }
        let client = self.client()?.clone();
        let scope = self.read_scope()?;
        let ticket = if let Some(conversation) = &scope.conversation {
            self.prepare_history(conversation)?;
            Some(
                self.history_state
                    .as_ref()
                    .context("对话历史尚未初始化")?
                    .begin_snapshot(),
            )
        } else {
            None
        };
        let id = uuid::Uuid::new_v4().to_string();
        let completed = self.read_completed.clone();
        let result_id = id.clone();
        let result_scope = scope.clone();
        let conversation = scope.conversation.clone();
        let lease_client = client.clone();
        let task = tokio::spawn(async move {
            let result = async {
                let home = client.read(ReadRequest::Home).await?;
                let nodes = client.read(ReadRequest::Nodes).await?;
                let workspace = client.read(ReadRequest::Workspace).await?;
                let write_state = client.read(ReadRequest::WriteState).await?;
                let questions = client.read(ReadRequest::Questions).await?;
                let notifications = client.read(ReadRequest::Notifications).await?;
                let conversation = match (conversation, ticket) {
                    (Some(conversation_id), Some(ticket)) => Some((
                        ticket,
                        client
                            .read(ReadRequest::Conversation { conversation_id })
                            .await?,
                    )),
                    _ => None,
                };
                Ok(RefreshReply {
                    home,
                    nodes,
                    workspace,
                    write_state,
                    questions,
                    notifications,
                    conversation,
                })
            }
            .await;
            let _ = completed
                .send(ReadCompleted {
                    id: result_id,
                    scope: result_scope,
                    result: ReadReply::Refresh(result.map(Box::new)),
                })
                .await;
        });
        self.reads.refresh = Some(PendingRead {
            id,
            scope,
            record: None,
            query: None,
            client: lease_client,
            joined: false,
            task,
        });
        Ok(())
    }
    pub(super) async fn start_history_page(
        &mut self,
        conversation: &str,
        section: vibeface_protocol::history::HistorySection,
    ) -> Result<()> {
        self.cancel_stale_reads().await?;
        ensure!(
            self.reads.page.is_none(),
            "已有历史分页正在读取，请等待原请求完成。"
        );
        ensure!(
            self.state.online && self.conversation.as_deref() == Some(conversation),
            "请先连接并打开原对话再读取历史"
        );
        self.prepare_history(conversation)?;
        let ticket = self
            .history_state
            .as_ref()
            .context("历史尚未首读")?
            .begin_page(section)?;
        let client = self.client()?.clone();
        let scope = self.read_scope()?;
        let id = uuid::Uuid::new_v4().to_string();
        let result_id = id.clone();
        let result_scope = scope.clone();
        let completed = self.read_completed.clone();
        let conversation = conversation.to_owned();
        let lease_client = client.clone();
        let task = tokio::spawn(async move {
            let result = client
                .read(ReadRequest::ConversationHistory(ticket.request()))
                .await;
            let _ = completed
                .send(ReadCompleted {
                    id: result_id,
                    scope: result_scope,
                    result: ReadReply::Page {
                        ticket,
                        conversation,
                        result,
                    },
                })
                .await;
        });
        self.reads.page = Some(PendingRead {
            id,
            scope,
            record: None,
            query: None,
            client: lease_client,
            joined: false,
            task,
        });
        self.state.history_loading = Some(section);
        self.state.history_error = None;
        self.publish();
        Ok(())
    }
    pub(super) async fn start_record_read(&mut self, query: RecordQuery) -> Result<()> {
        self.cancel_stale_reads().await?;
        let scope = self.read_scope()?;
        if self
            .reads
            .record
            .as_ref()
            .is_some_and(|pending| pending.scope == scope && pending.query.as_ref() == Some(&query))
        {
            self.reads.record_again = true;
            return Ok(());
        }
        stop_read(&mut self.reads.record).await?;
        self.reads.record_again = false;
        let client = self.client()?.clone();
        let id = uuid::Uuid::new_v4().to_string();
        let result_id = id.clone();
        let result_scope = scope.clone();
        let completed = self.read_completed.clone();
        let record = Some(RecordKey::from(&self.active_record));
        let pending_query = query.clone();
        let lease_client = client.clone();
        let task = tokio::spawn(async move {
            let result = async {
                match &query {
                    RecordQuery::History { channel, task } => Ok(RecordReply {
                        work: None,
                        history: Some(
                            client
                                .read(ReadRequest::WorkspaceHistory {
                                    channel_id: channel.clone(),
                                    task_id: task.clone(),
                                    limit: 100,
                                })
                                .await,
                        ),
                    }),
                    RecordQuery::Work { conversation, work } => {
                        let value = client
                            .read(ReadRequest::Work {
                                conversation_id: conversation.clone(),
                                work_id: work.clone(),
                            })
                            .await?;
                        let detail = crate::views::records::parse_work_detail(
                            value.clone(),
                            &result_scope.network,
                            conversation,
                            work,
                        )?;
                        let history = match &detail.work.target {
                            Some(InputTarget::Task { task_id }) => Some(
                                client
                                    .read(ReadRequest::WorkspaceHistory {
                                        channel_id: None,
                                        task_id: Some(task_id.clone()),
                                        limit: 100,
                                    })
                                    .await,
                            ),
                            Some(InputTarget::ChannelTask {
                                channel_id,
                                task_id,
                                ..
                            }) => Some(
                                client
                                    .read(ReadRequest::WorkspaceHistory {
                                        channel_id: Some(channel_id.clone()),
                                        task_id: Some(task_id.clone()),
                                        limit: 100,
                                    })
                                    .await,
                            ),
                            _ => None,
                        };
                        Ok(RecordReply {
                            work: Some(value),
                            history,
                        })
                    }
                }
            }
            .await;
            let _ = completed
                .send(ReadCompleted {
                    id: result_id,
                    scope: result_scope,
                    result: ReadReply::Record { query, result },
                })
                .await;
        });
        self.reads.record = Some(PendingRead {
            id,
            scope,
            record,
            query: Some(pending_query),
            client: lease_client,
            joined: false,
            task,
        });
        Ok(())
    }
    pub(super) async fn start_delivery_refresh(&mut self) -> Result<()> {
        self.cancel_stale_reads().await?;
        if self.reads.delivery.is_some() {
            self.reads.delivery_again = true;
            return Ok(());
        }
        let client = self.client()?.clone();
        let scope = self.read_scope()?;
        let id = uuid::Uuid::new_v4().to_string();
        let result_id = id.clone();
        let result_scope = scope.clone();
        let completed = self.read_completed.clone();
        let lease_client = client.clone();
        let task = tokio::spawn(async move {
            let result = async {
                let status = client
                    .read(ReadRequest::Delivery(DeliveryRead::Status))
                    .await?;
                let orders = client
                    .read(ReadRequest::Delivery(DeliveryRead::List))
                    .await?;
                let providers = client
                    .read(ReadRequest::Delivery(DeliveryRead::Providers))
                    .await?;
                let budgets = if matches!(
                    client.authority(),
                    vibeface_client::ClientAuthority::Membership { can_manage: true }
                ) && (status["paid_enabled"].as_bool() == Some(true)
                    || orders["orders"]
                        .as_array()
                        .is_some_and(|orders| orders.iter().any(|order| order["mode"] == "paid")))
                {
                    Some(
                        client
                            .read(ReadRequest::Delivery(DeliveryRead::Budgets))
                            .await,
                    )
                } else {
                    None
                };
                Ok(DeliveryReply {
                    status,
                    orders,
                    providers,
                    budgets,
                })
            }
            .await;
            let _ = completed
                .send(ReadCompleted {
                    id: result_id,
                    scope: result_scope,
                    result: ReadReply::Delivery(result),
                })
                .await;
        });
        self.reads.delivery = Some(PendingRead {
            id,
            scope,
            record: None,
            query: None,
            client: lease_client,
            joined: false,
            task,
        });
        Ok(())
    }
    pub(super) async fn finish_read(&mut self, completed: ReadCompleted) -> Result<()> {
        let pending = match &completed.result {
            ReadReply::Refresh(_) => &self.reads.refresh,
            ReadReply::Page { .. } => &self.reads.page,
            ReadReply::Record { .. } => &self.reads.record,
            ReadReply::Delivery(_) => &self.reads.delivery,
        };
        if pending.as_ref().is_none_or(|pending| {
            pending.id != completed.id
                || pending.scope != completed.scope
                || !self.read_is_current(pending)
        }) {
            return Ok(());
        }
        match completed.result {
            ReadReply::Refresh(result) => {
                finish_read_task(&mut self.reads.refresh).await?;
                let again = std::mem::take(&mut self.reads.refresh_again);
                let retry = self
                    .reads
                    .retry
                    .take()
                    .filter(|retry| retry.refresh_id == completed.id);
                // Retry authorization is this raw response, never the merged display cache.
                let fresh_proof = retry.as_ref().and_then(|retry| {
                    result.as_ref().ok().map(|reply| {
                        let (_, value) = reply
                            .conversation
                            .as_ref()
                            .context("最新响应没有原对话证明")?;
                        let bots: Vec<Bot> = decode_list(&reply.home, "bots")?;
                        let bot = bots
                            .iter()
                            .find(|bot| bot.conversation_id == retry.conversation)
                            .context("最新响应没有原 Bot")?;
                        parse_conversation(value.clone(), &retry.conversation, Some(&bot.bot_id))
                            .map(Arc::new)
                    })
                });
                let applied = match result {
                    Ok(reply) => self.apply_refresh(*reply).await,
                    Err(error) => {
                        self.state.online = false;
                        self.state.writable = false;
                        self.state.can_control = false;
                        self.state.status =
                            "连接中断，显示最近记录。结果未知的操作须先核对。".into();
                        self.state.error = Some(error.to_string());
                        self.publish();
                        Err(error)
                    }
                };
                if let Some(retry) = retry {
                    if matches!(&applied, Ok(true))
                        && self.state.online
                        && self.read_scope().is_ok_and(|scope| scope == retry.scope)
                        && matches!(&fresh_proof, Some(Ok(_)))
                    {
                        let command = Command::RetryReply {
                            conversation_id: retry.conversation.clone(),
                            message_id: retry.message.clone(),
                        };
                        self.reads.ready_retry =
                            Some((retry, fresh_proof.context("最新对话证明缺失")??));
                        let result = self.run(command).await;
                        self.reads.ready_retry = None;
                        result?;
                    } else {
                        let _ = self.emit(Event::Notice("最新状态未核对完成，原回复重试没有提交。恢复连接后请明确重新操作。".into())).await;
                    }
                }
                applied?;
                if again && self.client.as_ref().is_some_and(Client::is_online) {
                    self.start_refresh().await?;
                }
            }
            ReadReply::Page {
                ticket,
                conversation,
                result,
            } => {
                finish_read_task(&mut self.reads.page).await?;
                self.state.history_loading = None;
                let applied = self
                    .finish_history_page(&conversation, &ticket, result)
                    .await;
                self.publish();
                applied?;
            }
            ReadReply::Record { query, result } => {
                finish_read_task(&mut self.reads.record).await?;
                let again = std::mem::take(&mut self.reads.record_again);
                let applied = match result {
                    Ok(result) => self.apply_record_read(query.clone(), result).await,
                    Err(error) => Err(error),
                };
                if again {
                    self.start_record_read(query).await?;
                }
                applied?;
            }
            ReadReply::Delivery(result) => {
                finish_read_task(&mut self.reads.delivery).await?;
                let again = std::mem::take(&mut self.reads.delivery_again);
                let reply = result?;
                self.apply_delivery_refresh(
                    reply.status,
                    reply.orders,
                    reply.providers,
                    reply.budgets,
                )
                .await?;
                if again {
                    self.start_delivery_refresh().await?;
                }
            }
        }
        Ok(())
    }
    async fn apply_refresh(&mut self, reply: RefreshReply) -> Result<bool> {
        let client = self.client()?.clone();
        // Start from current UI/local state, not the state captured before the network wait.
        let mut next = self.state.clone();
        next.local_session = matches!(
            client.authority(),
            vibeface_client::ClientAuthority::LocalSession
        );
        next.write_state = reply.write_state["state"]
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
        next.bots = decode_list(&reply.home, "bots")?;
        next.nodes = decode_list(&reply.nodes, "nodes")?;
        next.tasks = decode_list(&reply.workspace, "tasks")?;
        next.assignments = decode_list(&reply.workspace, "assignments")?;
        next.channels = decode_list(&reply.workspace, "channels")?;
        next.inputs = decode_list(&reply.workspace, "inputs")?;
        next.questions = decode_list(&reply.questions, "questions")?;
        next.notifications = decode_list(&reply.notifications, "notifications")?;
        let mut fresh = true;
        if let Some((ticket, value)) = reply.conversation {
            let id = self.conversation.clone().context("对话已切换")?;
            let history = self.history_state.as_mut().context("对话历史已切换")?;
            match history.apply_snapshot(&ticket, value)? {
                ApplyResult::AuthorityReset => {
                    self.reset_history_authority().await?;
                    return Ok(false);
                }
                ApplyResult::Stale => {
                    fresh = false;
                }
                ApplyResult::Applied => {
                    let bot = next
                        .bots
                        .iter()
                        .find(|bot| bot.conversation_id == id)
                        .map(|bot| bot.bot_id.as_str());
                    next.conversation =
                        Arc::new(parse_conversation(history.materialize(), &id, bot)?);
                }
            }
        }
        next.drafts = load_draft_summaries(self.draft_dir()?).await?;
        next.pending = client
            .pending()
            .await?
            .into_iter()
            .map(|operation| Pending {
                id: operation.id,
                state: operation_label(operation.state).into(),
                target_node: operation.target_node,
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
        Ok(fresh)
    }
    async fn apply_record_read(&mut self, query: RecordQuery, result: RecordReply) -> Result<()> {
        let network = self.client()?.network_id().to_owned();
        let (channel, task) = match query {
            RecordQuery::History { channel, task } => (channel, task),
            RecordQuery::Work { conversation, work } => {
                let detail = crate::views::records::parse_work_detail(
                    result.work.context("工作详情缺失")?,
                    &network,
                    &conversation,
                    &work,
                )?;
                let target = detail.work.target.clone();
                self.state.work_detail = Some(Arc::new(detail));
                match target {
                    Some(InputTarget::Task { task_id }) => (None, Some(task_id)),
                    Some(InputTarget::ChannelTask {
                        channel_id,
                        task_id,
                        ..
                    }) => (Some(channel_id), Some(task_id)),
                    _ => (None, None),
                }
            }
        };
        if let Some(history) = result.history {
            match history.and_then(|value| {
                crate::views::records::parse_workspace_history(
                    value,
                    &network,
                    channel.as_deref(),
                    task.as_deref(),
                    &self.history_bindings(),
                )
            }) {
                Ok(history) => self.state.channel_history = Some(Arc::new(history)),
                Err(error) => {
                    let _ = self
                        .emit(Event::Notice(format!("原始附件历史暂不可读：{error}")))
                        .await;
                }
            }
        }
        self.publish();
        Ok(())
    }
}
