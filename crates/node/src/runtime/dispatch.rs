use super::*;
use vibeface_secure::authorization::{
    RequestFacts,
    execution_handoff::{ExecutionHandoffResult, SignedExecutionHandoff},
};

pub(super) fn execution_operation_view(
    store: &Store,
    operation: &OperationRecord,
) -> Result<Value> {
    let mut value = serde_json::to_value(operation)?;
    if let Some(wire) = store.execution_dispatch_handoff(
        &operation.request.network_id,
        &operation.request.operation_id,
    )? {
        value["execution_source"] = serde_json::to_value(wire.handoff.source_attestation)?;
    }
    Ok(value)
}

impl Runtime {
    pub(super) fn is_execution_metadata(&self, body: &Value) -> Result<bool> {
        let local = self.identity.node_id()?;
        Ok(matches!(
            body["method"].as_str(),
            Some("operation" | "session" | "reconcile")
        ) && body["target_node"]
            .as_str()
            .is_some_and(|target| target != local))
    }

    pub(super) fn is_execution_dispatch(&self, body: &Value) -> Result<bool> {
        let selected = ["execution_resource", "target_public_key", "execution_grant"];
        let target = match body["method"].as_str() {
            Some("operate") => body["request"]["target_node"].as_str(),
            Some("bind_execution") => body["target_node"].as_str(),
            _ => None,
        };
        let local = self.identity.node_id()?;
        let remote = target.is_some_and(|target| target != local);
        if remote {
            ensure!(
                selected.iter().all(|field| body.get(*field).is_some()),
                "remote execution requires an explicit destination"
            );
        } else {
            ensure!(
                selected.iter().all(|field| body.get(*field).is_none())
                    && body.get("execution_session_receipt").is_none(),
                "ordinary requests cannot select a remote execution destination"
            );
        }
        Ok(remote)
    }

    pub(super) fn authorization_facts(
        &self,
        store: &Store,
        body: &Value,
        audience: &Audience,
        now: u64,
    ) -> Result<RequestFacts> {
        if self.is_execution_metadata(body)? {
            if let Some(facts) = store.execution_dispatch_metadata_facts(body, audience, now)? {
                return Ok(facts);
            }
            store.request_facts(body, audience)
        } else if self.is_execution_dispatch(body)? {
            Ok(store
                .execution_dispatch_facts(body, audience, now)?
                .facts()
                .clone())
        } else {
            store.request_facts(body, audience)
        }
    }

    pub(super) async fn normalize_execution_dispatch(&self, mut body: Value) -> Result<Value> {
        if body["method"] != "operate" {
            return Ok(body);
        }
        let mut request: OperationRequest = serde_json::from_value(body["request"].clone())?;
        ensure!(
            request.network_id == field(&body, "space_id")?
                && body["authorization_id"] == request.operation_id,
            "operation space or authorization ID mismatch"
        );
        let store = self.store.lock().await;
        if request.binding.is_none() {
            let task = request
                .task_id
                .as_deref()
                .context("remote execution requires a task")?;
            let creation =
                store.task_creation(&request.network_id, &request.conversation_id, task)?;
            request.binding = Some(
                store
                    .task_binding(&request.network_id, &creation.hash)?
                    .context("confirm the remote task binding before operating")?,
            );
        }
        let binding = request
            .binding
            .as_ref()
            .context("remote task binding is missing")?;
        store.verify_historical_binding(binding)?;
        crate::store::validate_operation_binding_from(&store.connection, &request)?;
        if matches!(request.action["type"].as_str(), Some("open" | "send")) {
            request
                .resources
                .push(field(&binding.binding.open_spec, "cwd")?.to_owned());
        }
        request.resources.sort();
        request.resources.dedup();
        body["request"] = serde_json::to_value(request)?;
        Ok(body)
    }

    pub(super) async fn dispatch_execution(
        self: &Arc<Self>,
        access: &Access,
        request: OperationRequest,
    ) -> Result<Value> {
        {
            let mut store = self.store.lock().await;
            let facts = store.execution_dispatch_facts(
                &access.body,
                access.authorization.audience(),
                now_ms() as u64,
            )?;
            store.accept_execution_dispatch(
                &self.identity,
                &facts,
                &access.authorization,
                &access.freshness,
                &access.transport,
                &access.body,
                &request,
                now_ms() as u64,
            )?;
        }
        self.publish_changed(&request.network_id, &request.conversation_id, "");
        let key = (request.network_id.clone(), request.operation_id.clone());
        let mut refresh_error = None;
        if self
            .execution_dispatch_polls
            .lock()
            .await
            .insert(key.clone())
        {
            if let Err(error) = self.advance_execution_dispatch(&key.0, &key.1, true).await {
                refresh_error = Some(error.to_string());
            }
            let ongoing = self
                .store
                .lock()
                .await
                .operation(&key.0, &key.1)?
                .is_some_and(|operation| {
                    matches!(
                        operation.state,
                        OperationState::Accepted | OperationState::Running
                    )
                });
            if ongoing && refresh_error.is_none() {
                self.spawn_registered_execution_dispatch_poll(key, false);
            } else {
                self.execution_dispatch_polls.lock().await.remove(&key);
            }
        }
        let store = self.store.lock().await;
        let operation = store
            .operation(&request.network_id, &request.operation_id)?
            .context("durable Project execution metadata is missing")?;
        let operation = execution_operation_view(&store, &operation)?;
        Ok(
            json!({"execution_source":operation["execution_source"],"operation":operation,"refresh_error":refresh_error}),
        )
    }

    async fn request_execution_dispatch(
        &self,
        space: &str,
        target: &str,
        wire: &SignedExecutionHandoff,
        submit: bool,
    ) -> Result<ExecutionHandoffResult> {
        let kind = if submit {
            "execution_handoff_submit"
        } else {
            "execution_handoff_status"
        };
        let response = self
            .peer_request(space, target, json!({"type":kind,"handoff":wire}))
            .await?;
        let result: ExecutionHandoffResult = serde_json::from_value(response)?;
        ensure!(
            result.handoff == *wire,
            "execution response changed its durable handoff"
        );
        self.store.lock().await.verify_execution_dispatch_receipt(
            &self.identity,
            &result,
            now_ms() as u64,
        )?;
        Ok(result)
    }

    async fn advance_execution_dispatch(
        self: &Arc<Self>,
        space: &str,
        operation_id: &str,
        allow_first_submit: bool,
    ) -> Result<()> {
        let (wire, fresh, target) = {
            let mut store = self.store.lock().await;
            let operation = store
                .operation(space, operation_id)?
                .context("remote operation is missing")?;
            if !matches!(
                operation.state,
                OperationState::Accepted | OperationState::Running
            ) {
                return Ok(());
            }
            let (wire, fresh) = if allow_first_submit {
                store.prepare_execution_dispatch_handoff(
                    &self.identity,
                    space,
                    operation_id,
                    now_ms() as u64,
                )?
            } else {
                let Some(wire) = store.execution_dispatch_handoff(space, operation_id)? else {
                    return Ok(());
                };
                (wire, false)
            };
            (wire, fresh, operation.request.target_node)
        };
        let result = self
            .request_execution_dispatch(space, &target, &wire, fresh)
            .await;
        let mut store = self.store.lock().await;
        let operation = store
            .operation(space, operation_id)?
            .context("remote operation is missing")?;
        if !matches!(
            operation.state,
            OperationState::Accepted | OperationState::Running
        ) {
            return Ok(());
        }
        let applied = result.and_then(|result| {
            store.apply_execution_handoff_receipt(&self.identity, &result, now_ms() as u64)
        });
        match applied {
            Ok(operation) => {
                drop(store);
                self.publish_changed(space, &operation.request.conversation_id, "");
                if operation.state.terminal() {
                    self.complete_execution_dispatch(&operation).await?;
                }
                Ok(())
            }
            Err(error) => {
                store.mark_execution_dispatch_unknown(space, operation_id)?;
                self.publish_changed(space, &operation.request.conversation_id, "");
                Err(error
                    .context("remote execution requires reconciliation of its original handoff"))
            }
        }
    }

    fn spawn_registered_execution_dispatch_poll(
        self: &Arc<Self>,
        key: (String, String),
        immediately: bool,
    ) {
        let node = self.clone();
        tokio::spawn(async move {
            if !immediately {
                tokio::time::sleep(Duration::from_secs(3)).await;
            }
            loop {
                let work = node.maintenance.enter_when_open().await;
                let result = node.advance_execution_dispatch(&key.0, &key.1, false).await;
                let ongoing = node
                    .store
                    .lock()
                    .await
                    .operation(&key.0, &key.1)
                    .map(|operation| {
                        operation.is_some_and(|operation| {
                            matches!(
                                operation.state,
                                OperationState::Accepted | OperationState::Running
                            )
                        })
                    })
                    .unwrap_or(false);
                drop(work);
                if result.is_err() || !ongoing {
                    break;
                }
                tokio::time::sleep(Duration::from_secs(3)).await;
            }
            node.execution_dispatch_polls.lock().await.remove(&key);
        });
    }

    pub(super) async fn recover_execution_dispatches(self: &Arc<Self>) -> Result<()> {
        let local = self.identity.node_id()?;
        let candidates = {
            let store = self.store.lock().await;
            let mut candidates = vec![];
            for space in store.networks()? {
                for operation in store.operations(&space)? {
                    if operation.request.target_node != local
                        && matches!(
                            operation.state,
                            OperationState::Accepted | OperationState::Running
                        )
                        && store
                            .execution_dispatch_handoff(&space, &operation.request.operation_id)?
                            .is_some()
                    {
                        candidates.push((space.clone(), operation.request.operation_id));
                    }
                }
            }
            candidates
        };
        for key in candidates {
            if self
                .execution_dispatch_polls
                .lock()
                .await
                .insert(key.clone())
            {
                self.spawn_registered_execution_dispatch_poll(key, true);
            }
        }
        Ok(())
    }

    pub(super) async fn remote_operation_status(
        &self,
        access: &Access,
        target: &str,
        operation_id: &str,
    ) -> Result<Value> {
        {
            let store = self.store.lock().await;
            store.authorize_read(
                &access.authorization,
                &access.freshness,
                &access.transport,
                now_ms() as u64,
            )?;
            let operation = store
                .operation(&access.network_id, operation_id)?
                .context("remote operation is missing")?;
            ensure!(
                operation.request.target_node == target,
                "operation belongs to another execution target"
            );
        }
        let (operation, actual, error) = self
            .execution_dispatch_metadata(&access.network_id, operation_id)
            .await?;
        let operation = execution_operation_view(&*self.store.lock().await, &operation)?;
        Ok(
            json!({"execution_source":operation["execution_source"],"operation":operation,"remote_verified":actual.is_some(),"actual":actual,"refresh_error":error}),
        )
    }

    pub(super) async fn remote_session_status(
        &self,
        access: &Access,
        target: &str,
        session: &str,
    ) -> Result<Value> {
        let operation_id = field(&access.body, "operation_id")?;
        {
            let store = self.store.lock().await;
            store.authorize_read(
                &access.authorization,
                &access.freshness,
                &access.transport,
                now_ms() as u64,
            )?;
            let operation = store
                .operation(&access.network_id, operation_id)?
                .context("session has no authorized Project dispatch")?;
            ensure!(
                operation.request.target_node == target
                    && operation.request.session_id == session
                    && access
                        .authorization
                        .signed_invocation()
                        .payload
                        .context
                        .conversation
                        .as_ref()
                        .is_some_and(|scope| scope == &operation.request.conversation_id),
                "session differs from its authorized Project dispatch"
            );
        }
        let (operation, actual, error) = self
            .execution_dispatch_metadata(&access.network_id, operation_id)
            .await?;
        let operation = execution_operation_view(&*self.store.lock().await, &operation)?;
        Ok(
            json!({"session":actual.as_ref().map(|result| &result.receipt.payload.session),
            "receipt":actual.as_ref().map(|result| &result.receipt),"operation":operation,
            "execution_source":operation["execution_source"],"remote_verified":actual.is_some(),"actual":actual,"refresh_error":error}),
        )
    }

    async fn execution_dispatch_metadata(
        &self,
        space: &str,
        operation_id: &str,
    ) -> Result<(
        OperationRecord,
        Option<ExecutionHandoffResult>,
        Option<String>,
    )> {
        let (operation, wire) = {
            let store = self.store.lock().await;
            let operation = store
                .operation(space, operation_id)?
                .context("durable Project execution metadata is missing")?;
            (
                operation,
                store.execution_dispatch_handoff(space, operation_id)?,
            )
        };
        let Some(wire) = wire else {
            return Ok((
                operation,
                None,
                Some("remote operation has no durable submitted handoff".into()),
            ));
        };
        match self
            .request_execution_dispatch(space, &operation.request.target_node, &wire, false)
            .await
        {
            Ok(actual) => Ok((operation, Some(actual), None)),
            Err(error) => Ok((operation, None, Some(error.to_string()))),
        }
    }
}
