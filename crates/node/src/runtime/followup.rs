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
            attestation(args)?,
            now_ms(),
        )?;
        self.publish_changed(&access.network_id, scope, &envelope.hash);
        Ok(json!({"autonomy":self.store.lock().await.autonomy(&access.network_id,scope)?}))
    }

    /// The user moves a conversation's scheduling to another member Node. The new owner starts a
    /// fresh native session from the full history; queued turns follow it, finished ones stay.
    pub(super) async fn set_owner(
        &self,
        access: &Access,
        scope: &str,
        args: &Value,
    ) -> Result<Value> {
        ensure!(access.human, "only the user moves a conversation");
        self.require_scope(access, scope).await?;
        let network = &access.network_id;
        let owner = id(args, "scheduler_node")?;
        let epoch = args["epoch"].as_u64().context("epoch is required")?;
        let current = {
            let store = self.store.lock().await;
            store.authorize_node(network, &owner)?;
            store
                .conversation(network, scope)?
                .context("conversation not found")?
        };
        ensure!(
            epoch == current.owner_epoch + 1,
            "the conversation moved meanwhile; reload and try again"
        );
        let event = self
            .append(
                network,
                scope,
                EventBody::ConversationOwner {
                    scheduler_node: owner.clone(),
                    epoch,
                    attestation: Some(attestation(args)?),
                },
            )
            .await?;
        self.publish_changed(network, scope, &event.hash);
        self.abandon_revoked_works(network).await;
        self.schedule_wakeup.notify_one();
        self.outbox_wakeup.notify_one();
        Ok(json!({"conversation":self.store.lock().await.conversation(network, scope)?}))
    }
}
