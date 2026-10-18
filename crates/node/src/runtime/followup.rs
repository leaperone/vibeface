use super::*;

impl Runtime {
    pub(super) async fn set_followup(
        &self,
        access: &Access,
        scope: &str,
        args: &Value,
    ) -> Result<Value> {
        access.require_body(args)?;
        ensure!(
            access.account_actor(),
            "only an Account grants or revokes task follow-up"
        );
        self.require_scope(access, scope).await?;
        let task = field(args, "task_id")?;
        let (envelope, policy) = {
            let mut store = self.store.lock().await;
            let _verification = access.verification.enter();
            let envelope = store.set_task_followup(
                &self.identity,
                &access.authorization,
                &access.freshness,
                &access.transport,
                &access.body,
                now_ms(),
            )?;
            let policy = store.historical_followup_policy(&access.network_id, scope, task)?;
            (envelope, policy)
        };
        self.publish_changed(&access.network_id, scope, &envelope.hash);
        Ok(json!({"policy": policy, "event": envelope}))
    }
}
