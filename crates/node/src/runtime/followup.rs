use super::*;
use crate::store::followup::FollowupMode;

impl Runtime {
    pub(super) async fn set_followup(
        &self,
        access: &Access,
        scope: &str,
        args: &Value,
    ) -> Result<Value> {
        ensure!(
            access.human,
            "only the user grants or revokes task follow-up"
        );
        self.require_scope(access, scope).await?;
        self.require_role(&access.network_id, NodeRole::Gateway)
            .await?;
        let task = field(args, "task_id")?;
        let mode: FollowupMode = serde_json::from_value(args["mode"].clone())?;
        let expected = args["expected_version"]
            .as_u64()
            .context("expected_version is required")?;
        let target = args["scheduler_node"].as_str();
        let envelope = self.store.lock().await.set_task_followup(
            &self.identity,
            &access.network_id,
            scope,
            task,
            mode,
            expected,
            target,
            now_ms(),
        )?;
        self.publish_changed(&access.network_id, scope, &envelope.hash);
        Ok(
            json!({"policy":self.store.lock().await.followup_policy(&access.network_id,scope,task)?,"event":envelope}),
        )
    }
}
