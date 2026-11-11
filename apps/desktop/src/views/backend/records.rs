use super::*;

impl Backend {
    pub(super) async fn read_work_detail(
        &mut self,
        conversation_id: String,
        work_id: String,
    ) -> Result<()> {
        if self.conversation.as_ref() != Some(&conversation_id) {
            self.cancel_reads().await?;
            self.conversation = Some(conversation_id.clone());
            self.history_state = None;
            self.refresh().await?;
        }
        self.start_record_read(super::reads::RecordQuery::Work {
            conversation: conversation_id,
            work: work_id,
        })
        .await
    }
    pub(super) async fn read_history(
        &mut self,
        channel_id: Option<String>,
        task_id: Option<String>,
    ) -> Result<()> {
        self.start_record_read(super::reads::RecordQuery::History {
            channel: channel_id,
            task: task_id,
        })
        .await
    }
    pub(super) async fn refresh_active_record(&mut self) -> Result<()> {
        if !self.state.online {
            return Ok(());
        }
        self.last_record_read = tokio::time::Instant::now();
        match self.active_record.clone() {
            ActiveRecord::None => {}
            ActiveRecord::Work {
                conversation_id,
                work_id,
            } => self.read_work_detail(conversation_id, work_id).await?,
            ActiveRecord::Channel(channel_id) => self.read_history(Some(channel_id), None).await?,
            ActiveRecord::Task(task_id) => self.read_history(None, Some(task_id)).await?,
        }
        Ok(())
    }
}
