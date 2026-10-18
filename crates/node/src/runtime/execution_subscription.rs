use super::*;

impl Runtime {
    pub(super) async fn restore_execution_subscriptions(self: &Arc<Self>) -> Result<()> {
        let _work = self.maintenance.enter()?;
        let local = self.identity.node_id()?;
        let (threads, operations) = {
            let store = self.store.lock().await;
            let mut threads = BTreeSet::new();
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
                    threads.insert(native_id.clone());
                    if store
                        .local_value(&network, &format!("turn:{}", request.operation_id))?
                        .is_some_and(|turn| turn["turn"]["id"].is_string())
                    {
                        operations.push((network.clone(), request.operation_id.clone(), native_id));
                    }
                }
            }
            (threads, operations)
        };
        self.engine
            .set_observed_threads(threads.into_iter().collect())?;
        let snapshot = self.engine.snapshot().await;
        if snapshot["connection"]["state"] != "connected" {
            return Ok(());
        }
        for (network, operation, native_id) in operations {
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
