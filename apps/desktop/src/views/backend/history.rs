use super::*;
use crate::views::history::{ApplyResult, HistoryScope, HistoryState};
use vibeface_protocol::history::HistorySection;
impl Backend {
    fn history_scope(&self, id: &str) -> Result<HistoryScope> {
        let client = self.client()?;
        Ok(HistoryScope {
            network_id: client.network_id().into(),
            client_id: client.client_id().into(),
            conversation_id: id.into(),
            generation: self.state.generation,
        })
    }
    fn prepare_history(&mut self, id: &str) -> Result<()> {
        let scope = self.history_scope(id)?;
        if let Some(history) = &mut self.history_state {
            let old = history.scope();
            if old == &scope {
                return Ok(());
            }
        }
        self.history_state = Some(Box::new(HistoryState::new(scope)));
        Ok(())
    }
    pub(super) async fn read_conversation(
        &mut self,
        id: &str,
        bot: Option<&str>,
    ) -> Result<(Conversation, bool)> {
        self.prepare_history(id)?;
        let ticket = self
            .history_state
            .as_ref()
            .context("对话历史尚未初始化")?
            .begin_snapshot();
        let value = self
            .client()?
            .read(ReadRequest::Conversation {
                conversation_id: id.into(),
            })
            .await?;
        let history = self.history_state.as_mut().context("对话历史已切换")?;
        let result = history.apply_snapshot(&ticket, value)?;
        let conversation = parse_conversation(history.materialize(), id, bot)?;
        Ok((conversation, result == ApplyResult::AuthorityReset))
    }
    pub(super) async fn merge_history_push(&mut self, value: Value) -> Result<()> {
        let id = self.conversation.clone().context("尚未选择对话")?;
        self.prepare_history(&id)?;
        let refresh = value["refresh_metadata"].as_bool().unwrap_or(false);
        let history = self.history_state.as_mut().context("对话历史尚未初始化")?;
        match history.apply_push(value)? {
            ApplyResult::AuthorityReset => {
                self.reset_history_authority().await?;
                return Ok(());
            }
            ApplyResult::Stale => return Ok(()),
            ApplyResult::Applied => {}
        }
        let bot = self
            .state
            .bots
            .iter()
            .find(|bot| bot.conversation_id == id)
            .map(|bot| bot.bot_id.as_str());
        self.state.conversation = Arc::new(parse_conversation(history.materialize(), &id, bot)?);
        prune_terminal_progress(&mut self.state.live, &self.state.conversation);
        self.publish();
        if refresh {
            self.refresh().await?;
        } else {
            self.save_projection().await?;
            self.refresh_active_record().await?;
        }
        Ok(())
    }
    pub(super) async fn load_history(&mut self, id: &str, section: HistorySection) -> Result<()> {
        ensure!(
            self.state.online && self.conversation.as_deref() == Some(id),
            "请先连接并打开原对话再读取历史"
        );
        self.prepare_history(id)?;
        let ticket = self
            .history_state
            .as_ref()
            .context("历史尚未首读")?
            .begin_page(section)?;
        self.state.history_loading = Some(section);
        self.state.history_error = None;
        self.publish();
        let response = self
            .client()?
            .read(ReadRequest::ConversationHistory(ticket.request()))
            .await;
        self.state.history_loading = None;
        match response {
            Ok(value) => {
                let history = self.history_state.as_mut().context("对话历史已切换")?;
                match history.apply_page(&ticket, value)? {
                    ApplyResult::Stale => {
                        self.state.history_error =
                            Some("历史请求已过期，当前视图保持原样。".into());
                        self.publish();
                        return Ok(());
                    }
                    ApplyResult::AuthorityReset => {
                        self.reset_history_authority().await?;
                        return Ok(());
                    }
                    ApplyResult::Applied => {}
                }
                let bot = self
                    .state
                    .bots
                    .iter()
                    .find(|bot| bot.conversation_id == id)
                    .map(|bot| bot.bot_id.as_str());
                self.state.conversation =
                    Arc::new(parse_conversation(history.materialize(), id, bot)?);
                self.publish();
                self.save_projection().await?;
            }
            Err(error) => {
                self.state.history_error = Some(format!(
                    "较早历史未读取：{error}。重新读取最新窗口后再继续。"
                ));
                self.publish();
                self.refresh().await?;
            }
        }
        Ok(())
    }
    pub(super) async fn restore_profile(
        &mut self,
        id: &str,
        record: &crate::views::history::OverwrittenRecord,
    ) -> Result<()> {
        ensure!(
            self.conversation.as_deref() == Some(id) && self.state.can_stage_input(),
            "原对话或成员状态已改变，请重新读取后再恢复"
        );
        let expected = serde_json::to_value(record)?;
        ensure!(
            self.state
                .conversation
                .overwritten
                .iter()
                .any(|current| serde_json::to_value(current).ok().as_ref() == Some(&expected)),
            "覆盖记录已改变，请重新核对当前值"
        );
        let original = self
            .state
            .conversation
            .changes
            .iter()
            .find(|change| change.change_id == record.change_id)
            .context("尚未读取原始变更，请继续读取配置变更历史")?;
        let restore = record.restoration(original)?;
        let bot = self
            .state
            .bots
            .iter()
            .find(|bot| bot.conversation_id == id)
            .context("原Bot尚未读取")?;
        let bot_id = bot.bot_id.clone();
        let version = bot.avatar_version.clone();
        match restore {
            crate::views::history::ProfileRestoration::Avatar(avatar) => {
                let avatar = avatar
                    .map(serde_json::to_value)
                    .transpose()?
                    .map(serde_json::from_value)
                    .transpose()?;
                self.submit(Action::SetBotAvatar {
                    conversation_id: id.into(),
                    version,
                    avatar,
                })
                .await?;
            }
            crate::views::history::ProfileRestoration::Instruction(text) => {
                self.input_instruction(id.into(), bot_id, text).await?
            }
        }
        Ok(())
    }
    pub(super) async fn reset_history_authority(&mut self) -> Result<()> {
        if let Some(history) = &mut self.history_state {
            history.invalidate();
        }
        self.state.conversation = Arc::new(Conversation::default());
        self.state.live.clear();
        self.state.synced = false;
        self.state.cached = false;
        self.state.projection_valid = false;
        self.state.online = false;
        self.state.writable = false;
        self.state.can_save_locally = false;
        self.state.can_control = false;
        self.state.can_manage = false;
        self.state.history_loading = None;
        self.state.history_error =
            Some("成员授权或历史投影已改变，旧行已清空，正在重新验证。".into());
        if let Some(client) = &self.client {
            client.close();
        }
        self.retry_at = tokio::time::Instant::now();
        self.publish();
        self.invalidate_projection().await?;
        Ok(())
    }
}
