use super::*;

impl Runtime {
    pub(super) async fn restore_execution_subscriptions(self: &Arc<Self>) -> Result<()> {
        let _work = self.maintenance.enter()?;
        let local = self.identity.node_id()?;
        let operations = {
            let store = self.store.lock().await;
            let mut operations = Vec::new();
            for network in store.networks()? {
                if store
                    .authorize_node(&network, &local, Some(NodeRole::Executor))
                    .is_err()
                {
                    continue;
                }
                for operation in store.operations(&network)? {
                    let request = &operation.request;
                    if request.target_node != local
                        || request.action["type"] != "send"
                        || !matches!(
                            operation.state,
                            OperationState::Running | OperationState::Unknown
                        )
                    {
                        continue;
                    }
                    let Some(session_id) = request.session_id.as_deref() else {
                        continue;
                    };
                    let Some(session) = store.session(&network, &local, session_id)? else {
                        continue;
                    };
                    if session.executor != "codex" {
                        continue;
                    }
                    let Some(native_id) = session.native_session_id else {
                        continue;
                    };
                    if store
                        .local_value(&network, &format!("turn:{}", request.operation_id))?
                        .is_some_and(|turn| turn["turn"]["id"].is_string())
                    {
                        let context = execution_context(&store, request, 0)?;
                        operations.push((
                            network.clone(),
                            request.operation_id.clone(),
                            native_id,
                            context,
                        ));
                    }
                }
            }
            operations
        };
        for (network, operation, native_id, context) in operations {
            if let Err(error) = self
                .engine
                .restore_observed_thread(&context, &native_id)
                .await
            {
                tracing::debug!(%error, operation_id=%operation, "execution observation pending");
                continue;
            }
            let snapshot = self.engine.snapshot_authorized(&context).await?;
            if snapshot["connection"]["state"] != "connected" {
                continue;
            }

            let thread = &snapshot["threads"][&native_id];
            if thread.is_null() || thread["status"]["type"] == "active" {
                continue;
            }
            if let Err(error) = self.reconcile(&network, &local, &operation).await {
                tracing::debug!(%error, operation_id=%operation, "execution subscription reconciliation pending");
            }
        }
        Ok(())
    }
}
