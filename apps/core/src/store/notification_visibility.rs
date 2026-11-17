use super::Store;
use crate::model::{Event, EventBody, Notification};
use anyhow::Result;

impl Store {
    pub(crate) fn notification_visible(
        &self,
        network: &str,
        notification: &Notification,
    ) -> Result<bool> {
        if notification.kind != "question" {
            return Ok(true);
        }
        if let Some(event) = self.event(network, &notification.notification_id)?
            && let EventBody::QuestionAsked {
                question_id, about, ..
            } = serde_json::from_str::<Event>(&event.canonical_body)?.body
        {
            if let Some(question) = self.question(network, &question_id)?
                && (!question["answer"].is_null() || !question["closed"].is_null())
            {
                return Ok(false);
            }
            if self.session_muted(network, &about.node, &about.session_id)? {
                return Ok(false);
            }
        }
        Ok(true)
    }

    pub(crate) fn display_notifications(
        &self,
        network: &str,
        client: &str,
    ) -> Result<Vec<Notification>> {
        let mut visible = Vec::new();
        for notification in self.pending_notifications(network, client)? {
            if self.notification_visible(network, &notification)? {
                visible.push(notification);
            }
        }
        Ok(visible)
    }
}
