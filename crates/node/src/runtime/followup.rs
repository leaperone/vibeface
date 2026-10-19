use super::*;

impl Runtime {
    /// The user's choice for work results: `continue`, `report` (read-only turn) or `off`.
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
        let mode: AutonomyMode = serde_json::from_value(args["mode"].clone())?;
        let expected = args["expected_version"]
            .as_u64()
            .context("expected_version is required")?;
        let envelope = self.store.lock().await.set_autonomy(
            &self.identity,
            &access.network_id,
            scope,
            mode,
            expected,
            now_ms(),
        )?;
        self.publish_changed(&access.network_id, scope, &envelope.hash);
        Ok(json!({"autonomy":self.store.lock().await.autonomy(&access.network_id,scope)?}))
    }
}
