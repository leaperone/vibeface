use super::*;
use crate::store::followup::FollowupMode;

impl Runtime {
    pub(super) async fn set_autonomy(
        &self,
        access: &Access,
        scope: &str,
        args: &Value,
    ) -> Result<Value> {
        ensure!(access.human, "only the user changes conversation autonomy");
        self.require_scope(access, scope).await?;
        self.require_role(&access.network_id, NodeRole::Gateway)
            .await?;
        let mode: FollowupMode = serde_json::from_value(args["mode"].clone())?;
        let expected = args["expected_version"]
            .as_u64()
            .context("expected_version is required")?;
        let mut target = args["scheduler_node"]
            .as_str()
            .filter(|value| !value.is_empty())
            .map(String::from);
        if target.is_none()
            && mode != FollowupMode::Off
            && self
                .store
                .lock()
                .await
                .conversation_autonomy(&access.network_id, scope)?
                .target_node
                .is_empty()
        {
            target = Some(
                self.select_chat_scheduler(&access.network_id, scope)
                    .await?,
            );
        }
        let envelope = self.store.lock().await.set_conversation_autonomy(
            &self.identity,
            &access.network_id,
            scope,
            mode,
            expected,
            target.as_deref(),
            now_ms(),
        )?;
        self.publish_changed(&access.network_id, scope, &envelope.hash);
        Ok(
            json!({"autonomy":self.store.lock().await.conversation_autonomy(&access.network_id,scope)?,"event":envelope}),
        )
    }
}
