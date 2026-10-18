use super::*;
use crate::identity::task_business_policy::{
    TaskBusinessPolicySourceContext, TaskBusinessRequest, TaskBusinessResponse,
};
use vibeface_secure::authorization::{
    RequestFacts,
    execution_handoff::{
        ExecutionHandoffResult, SignedExecutionHandoff, SignedExecutionHandoffResolution,
        VerifiedExecutionHandoff,
    },
    task_business_policy::{
        TaskBusinessAuthorization, TaskBusinessWire, receipt::TaskBusinessChallenge,
    },
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
    if let Some(dispatch) = store.operation_dispatch(
        &operation.request.network_id,
        &operation.request.operation_id,
    )? && operation.request.target_node
        == serde_json::from_str::<Event>(&dispatch.canonical_body)?.author
    {
        value["project_execution_origin"] = json!(store.local_project_dispatch_origin(&dispatch)?);
        value["project_dispatch"] = json!(dispatch);
        value["project_execution_result"] = json!(store.local_project_execution_result(
            &operation.request.network_id,
            &operation.request.operation_id
        )?);
    }
    Ok(value)
}

fn completed_execution_resolution(
    store: &Store,
    wire: &SignedExecutionHandoffResolution,
) -> Result<Option<OperationRecord>> {
    use vibeface_secure::authorization::execution_handoff::{
        ExecutionHandoffResolutionResult, ExecutionHandoffTerminalReconciliationResult,
        ExecutionResolutionPurpose,
    };
    let tx = store.connection.unchecked_transaction()?;
    let operation = store
        .operation(
            wire.resolution
                .account_admission
                .invocation
                .payload
                .resource
                .space
                .as_str(),
            &wire.resolution.observation.receipt.payload.operation_id,
        )?
        .context("prepared resolution lost its original operation")?;
    if !operation.state.terminal() {
        return Ok(None);
    }
    let recorded = match wire.resolution.purpose {
        ExecutionResolutionPurpose::ResolveUnknown => {
            serde_json::from_value::<ExecutionHandoffResolutionResult>(operation.result.clone())?
                .resolution
        }
        ExecutionResolutionPurpose::ReconcileTerminal => {
            serde_json::from_value::<ExecutionHandoffTerminalReconciliationResult>(
                operation.result.clone(),
            )?
            .reconciliation
        }
    };
    ensure!(
        &recorded == wire,
        "completed operation belongs to another resolution authorization"
    );
    crate::store::validate_released_operation_reservations_from(
        &tx,
        &operation.request.network_id,
        &operation.request.operation_id,
    )?;
    tx.commit()?;
    Ok(Some(operation))
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

    pub(super) fn is_execution_resolution(&self, body: &Value) -> Result<bool> {
        let local = self.identity.node_id()?;
        Ok(body["method"] == "resolve_unknown"
            && body["target_node"]
                .as_str()
                .is_some_and(|target| target != local))
    }

    pub(super) fn is_remote_execution_seal(&self, body: &Value) -> bool {
        matches!(
            body["method"].as_str(),
            Some("seal_remote_execution" | "remote_execution_seal_status")
        )
    }

    pub(super) fn check_execution_actor(
        &self,
        body: &Value,
        invocation: &Invocation,
    ) -> Result<()> {
        if self.is_remote_execution_seal(body) || body["method"] == "rebind_execution" {
            ensure!(
                matches!(invocation.actor, Actor::Account(_))
                    && invocation.initiator == invocation.actor
                    && invocation.initiating_grant.is_none()
                    && invocation.context.sdk_session.is_none()
                    && body["sdk_session"].is_null()
                    && body["parent_invocation"].is_null(),
                "execution rebind and seal require an independent account action"
            );
            return Ok(());
        }
        let resolution = self.is_execution_resolution(body)?;
        if !resolution && !self.is_execution_metadata(body)? {
            return Ok(());
        }
        match invocation.actor {
            Actor::Account(_) => ensure!(
                invocation.context.sdk_session.is_none()
                    && body["sdk_session"].is_null()
                    && body["parent_invocation"].is_null(),
                "account metadata cannot claim an SDK session"
            ),
            Actor::Service(_) => ensure!(
                !resolution
                    && invocation.context.sdk_session.is_some()
                    && serde_json::to_value(&invocation.context.sdk_session)?
                        == body["sdk_session"]
                    && body["parent_invocation"].as_str().is_some(),
                "service metadata requires its actual admitted SDK session and parent"
            ),
        }
        Ok(())
    }

    pub(super) fn is_execution_dispatch(&self, body: &Value) -> Result<bool> {
        let selected = ["execution_resource", "target_public_key", "execution_grant"];
        let target = match body["method"].as_str() {
            Some("operate") => body["request"]["target_node"].as_str(),
            Some("bind_execution" | "rebind_execution") => body["target_node"].as_str(),
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

    pub(super) fn authorization_facts_in_scope(
        &self,
        store: &Store,
        authorization: &crate::identity::task_execution_context::TaskScopedAuthorization,
        body: &Value,
        audience: &Audience,
        now: u64,
    ) -> Result<RequestFacts> {
        if self.is_execution_resolution(body)? || self.is_execution_dispatch(body)? {
            return Ok(store
                .execution_dispatch_facts_in_scope(authorization, body, audience, now)?
                .facts()
                .clone());
        }
        self.authorization_facts(store, body, audience, now)
    }

    pub(super) fn authorization_facts(
        &self,
        store: &Store,
        body: &Value,
        audience: &Audience,
        now: u64,
    ) -> Result<RequestFacts> {
        if self.is_remote_execution_seal(body) {
            store.remote_execution_seal_facts(body, audience, now)
        } else if self.is_execution_metadata(body)? {
            if let Some(facts) = store.execution_dispatch_metadata_facts(body, audience, now)? {
                return Ok(facts);
            }
            ensure!(
                body["sdk_session"].is_null(),
                "SDK metadata requires its original durable Project dispatch"
            );
            store.request_facts(body, audience)
        } else if self.is_execution_resolution(body)? || self.is_execution_dispatch(body)? {
            Ok(store
                .execution_dispatch_facts(body, audience, now)?
                .facts()
                .clone())
        } else {
            store.request_facts(body, audience)
        }
    }

    pub(super) async fn execution_tool_targets(
        &self,
        session: &AgentSession,
        body: &Value,
        verification: &SignatureVerification,
    ) -> Result<Vec<Value>> {
        let (Some(task), Some(executor), Some(action)) = (
            body["task_id"].as_str(),
            body["executor"].as_str(),
            body["execution_action"].as_str(),
        ) else {
            return Ok(vec![]);
        };
        ensure!(
            matches!(action, "open" | "send"),
            "target discovery requires an explicit open or send action"
        );
        let store = self.store.lock().await;
        verification.run(|| {
            self.bound_sdk_execution_tool_targets(&store, session, task, executor, now_ms() as u64)
        })
    }

    pub(super) async fn prepare_execution_tool_body(
        &self,
        session: &AgentSession,
        tool: &str,
        args: &Value,
        body: &mut Value,
        verification: &SignatureVerification,
    ) -> Result<()> {
        ensure!(
            [
                "execution_session_receipt",
                "execution_source",
                "binding",
                "native_session_id"
            ]
            .iter()
            .all(|field| args.get(*field).is_none()),
            "execution tools accept stored authorization identifiers, not session receipts or native handles"
        );
        let target = if body["method"] == "operate" {
            field(&body["request"], "target_node")?
        } else {
            field(body, "target_node")?
        }
        .to_owned();
        let selected = ["execution_resource", "target_public_key", "execution_grant"];
        if target == self.identity.node_id()? {
            ensure!(
                selected.iter().all(|field| args.get(*field).is_none())
                    && args.get("prior_operation_id").is_none(),
                "local execution tools cannot select a remote destination or prior remote operation"
            );
            return Ok(());
        }
        if tool == "execution_status" {
            let operation_id = id(args, "operation_id")?;
            let store = self.store.lock().await;
            return verification.run(|| {
                let operation = store
                    .operation(session.space_id.as_str(), &operation_id)?
                    .context("remote status requires a known original Project operation ID")?;
                self.check_execution_tool_operation(
                    &store,
                    session,
                    &operation,
                    &target,
                    field(body, "session_id")?,
                    None,
                )?;
                let wire = store
                    .execution_dispatch_handoff(session.space_id.as_str(), &operation_id)?
                    .context("remote status requires the original durable execution handoff")?;
                let binding = operation
                    .request
                    .binding
                    .as_ref()
                    .context("remote operation has no signed task binding")?;
                body["operation_id"] = json!(operation_id);
                body["execution_source"] = serde_json::to_value(
                    wire.handoff
                        .source_attestation
                        .context("remote operation has no signed Project source")?,
                )?;
                body["binding"] = serde_json::to_value(binding)?;
                body["task_id"] = json!(operation.request.task_id);
                body["task_ref"] = json!(binding.binding.task_ref);
                Ok(())
            });
        }
        for field in selected {
            body[field] = args
                .get(field)
                .with_context(|| format!("remote execution requires the authorized {field}"))?
                .clone();
        }
        if body["method"] == "operate" {
            *body = self
                .normalize_execution_dispatch_with_verification(body.clone(), verification)
                .await?;
        }
        self.ensure_sdk_execution_task_policy(session, body).await?;
        if tool != "send_execution" {
            return Ok(());
        }
        let prior_id = id(args, "prior_operation_id")?;
        let request: OperationRequest = serde_json::from_value(body["request"].clone())?;
        let (wire, binding, prior_state) = {
            let store = self.store.lock().await;
            verification.run(|| -> Result<_> {
                let operation = store
                    .operation(session.space_id.as_str(), &prior_id)?
                    .context("remote send requires a known original prior operation ID")?;
                self.check_execution_tool_operation(
                    &store,
                    session,
                    &operation,
                    &target,
                    request
                        .session_id
                        .as_deref()
                        .context("SDK operation requires a session")?,
                    request.task_id.as_deref(),
                )?;
                ensure!(
                    operation.state.terminal(),
                    "reconcile the original remote operation before sending another task"
                );
                let task = request
                    .task_id
                    .as_deref()
                    .context("remote send requires its original task")?;
                let creation = store.task_creation(
                    session.space_id.as_str(),
                    &session.conversation_id,
                    task,
                )?;
                let binding = store
                    .task_binding(session.space_id.as_str(), &creation.hash)?
                    .context("remote send requires its confirmed signed task binding")?;
                ensure!(
                    operation.request.binding.as_ref() == Some(&binding),
                    "prior operation belongs to another task binding generation"
                );
                let wire = store
                    .execution_dispatch_handoff(session.space_id.as_str(), &prior_id)?
                    .context("prior operation has no durable execution handoff")?;
                ensure!(
                    serde_json::to_value(&wire.handoff.execution_resource)?
                        == body["execution_resource"]
                        && serde_json::to_value(&wire.handoff.target_public_key)?
                            == body["target_public_key"],
                    "remote send changed the prior operation's executor resource or public key"
                );
                Ok((wire, binding, operation.state))
            })?
        };
        let actual = self
            .request_execution_dispatch_with_verification(
                session.space_id.as_str(),
                &target,
                &wire,
                false,
                verification,
            )
            .await?;
        ensure!(
            serde_json::to_value(actual.receipt.payload.state)?
                == serde_json::to_value(prior_state)?,
            "executor status differs from the original terminal operation"
        );
        let project = ResourceRef {
            space: session.space_id.clone(),
            resource: session.resource_id.clone(),
        };
        let project_key = PublicKey::parse(&self.identity.public_key_hex()?)?;
        let snapshot = verification.run(|| {
            vibeface_secure::authorization::execution_handoff::verify_execution_session_snapshot(
                &actual.receipt,
                &project,
                &wire.handoff.execution_resource,
                &wire.handoff.target_public_key,
                &binding,
                &project_key,
                now_ms() as u64,
            )
        })?;
        ensure!(
            !matches!(
                snapshot.state.as_str(),
                "unknown" | "closed" | "opening" | "resolved"
            ),
            "remote session is unresolved or closed; confirm a new binding and session before sending"
        );
        ensure!(
            snapshot.revision == request.expected_revision,
            "remote session revision changed; read its actual status before sending"
        );
        body["execution_session_receipt"] = serde_json::to_value(actual.receipt)?;
        Ok(())
    }

    fn check_execution_tool_operation(
        &self,
        store: &Store,
        session: &AgentSession,
        operation: &OperationRecord,
        target: &str,
        execution_session: &str,
        task: Option<&str>,
    ) -> Result<()> {
        let request = &operation.request;
        let original =
            store.operation_admission_proof(session.space_id.as_str(), &request.operation_id)?;
        ensure!(
            request.network_id == session.space_id.as_str()
                && request.conversation_id == session.conversation_id
                && request.target_node == target
                && request.session_id.as_deref() == Some(execution_session)
                && original.invocation.payload.resource.space == session.space_id
                && original.invocation.payload.resource.resource == session.resource_id
                && original.invocation.payload.initiator == session.initiator
                && task.is_none_or(|task| request.task_id.as_deref() == Some(task)),
            "remote operation belongs to another Project, task or execution session"
        );
        let origin = store.agent_origin(session, now_ms() as u64)?;
        if let Some((task, reference)) = origin.task {
            ensure!(
                request.task_id.as_deref() == Some(task.as_str())
                    && request
                        .binding
                        .as_ref()
                        .is_some_and(|binding| binding.binding.task_ref == reference.as_str()),
                "automatic continuation cannot inspect another task's remote operation"
            );
        }
        if let Some(original) = origin.execution {
            ensure!(
                request.target_node == original.target_node
                    && request.session_id == original.session_id
                    && request.binding == original.binding,
                "automatic continuation cannot inspect another execution binding or session"
            );
        }
        Ok(())
    }

    pub(super) async fn normalize_execution_dispatch_with_verification(
        &self,
        mut body: Value,
        verification: &SignatureVerification,
    ) -> Result<Value> {
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
        verification.run(|| -> Result<Value> {
            if let Some(task) = request.task_id.as_deref() {
                if request.binding.is_none() {
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
                    .context("remote Task binding is missing")?;
                store.verify_historical_binding(binding)?;
                if matches!(request.action["type"].as_str(), Some("open" | "send")) {
                    request
                        .resources
                        .push(field(&binding.binding.open_spec, "cwd")?.to_owned());
                }
            } else {
                ensure!(
                    request.binding.is_none(),
                    "ordinary remote execution cannot claim a Task binding"
                );
                if matches!(request.action["type"].as_str(), Some("open" | "command")) {
                    let cwd = field(&request.action, "cwd")?;
                    crate::identity::execution_dispatch::validate_remote_resource_path(cwd)?;
                    request.resources.push(cwd.to_owned());
                } else if request.action["type"] == "send" {
                    let receipt: Signed<
                        vibeface_secure::authorization::execution_handoff::ExecutionHandoffReceipt,
                    > = serde_json::from_value(body["execution_session_receipt"].clone())?;
                    let snapshot = receipt
                        .payload
                        .session
                        .context("remote continuation requires its signed session snapshot")?;
                    request.resources.push(snapshot.cwd);
                }
            }
            crate::store::validate_operation_binding_from(&store.connection, &request)?;
            request.resources.sort();
            request.resources.dedup();
            body["request"] = serde_json::to_value(request)?;
            Ok(body)
        })
    }

    pub(super) async fn dispatch_execution(
        self: &Arc<Self>,
        access: &Access,
        request: OperationRequest,
    ) -> Result<Value> {
        {
            let mut store = self.store.lock().await;
            access.verification.run(|| -> Result<()> {
                let facts = store.execution_dispatch_facts_in_scope(
                    &access.authorization,
                    &access.body,
                    access.authorization.audience(),
                    now_ms() as u64,
                )?;
                store.accept_execution_request_dispatch(
                    &self.identity,
                    &facts,
                    &access.authorization,
                    &access.freshness,
                    &access.transport,
                    &access.body,
                    &request,
                    now_ms() as u64,
                )?;
                Ok(())
            })?;
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
            if let Err(error) = self
                .advance_execution_dispatch_with_verification(
                    &key.0,
                    &key.1,
                    true,
                    &access.verification,
                )
                .await
            {
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

    async fn request_execution_dispatch_with_verification(
        &self,
        space: &str,
        target: &str,
        wire: &SignedExecutionHandoff,
        submit: bool,
        verification: &SignatureVerification,
    ) -> Result<ExecutionHandoffResult> {
        let source = {
            let mut store = self.store.lock().await;
            SignatureVerification::new().run(|| {
                store.prepare_execution_dispatch_task_business_source(
                    &self.identity,
                    wire,
                    now_ms() as u64,
                )
            })?
        };
        let response = if let Some((source, authorization)) = source {
            ensure!(
                source.header().payload.scope.source_project.space.as_str() == space
                    && crypto::fingerprint(source.target_public_key().as_str())? == target,
                "Task business source differs from its requested Project or target"
            );
            Box::pin(self.request_execution_dispatch_task_business(
                source,
                authorization,
                wire,
                submit,
            ))
            .await?
        } else {
            let kind = if submit {
                "execution_handoff_submit"
            } else {
                "execution_handoff_status"
            };
            self.peer_request(space, target, json!({"type":kind,"handoff":wire}))
                .await?
        };
        let result: ExecutionHandoffResult = serde_json::from_value(response)?;
        ensure!(
            result.handoff == *wire,
            "execution response changed its durable handoff"
        );
        let store = self.store.lock().await;
        verification.run(|| {
            store.verify_execution_dispatch_receipt(&self.identity, &result, now_ms() as u64)
        })?;
        Ok(result)
    }

    async fn request_execution_dispatch_task_business(
        &self,
        mut source: TaskBusinessPolicySourceContext,
        authorization: VerifiedExecutionHandoff,
        wire: &SignedExecutionHandoff,
        submit: bool,
    ) -> Result<Value> {
        let mut client = self.open_task_business_client(&mut source).await?;
        let header_hash = source.header().hash()?;
        let business = TaskBusinessWire::Submit(wire.clone());
        let wire_hash = business.hash()?;
        let request = if submit {
            TaskBusinessRequest::Business(business.clone())
        } else {
            TaskBusinessRequest::Status {
                wire: business.clone(),
            }
        };
        let mut response = client.exchange(&request).await?;
        if let TaskBusinessResponse::Challenge {
            source_context,
            business_wire_hash,
            challenge,
        } = &response
        {
            let TaskBusinessChallenge::Submit(typed) = challenge else {
                bail!("Task business submission returned another challenge purpose");
            };
            ensure!(
                *source_context == header_hash
                    && *business_wire_hash == wire_hash
                    && typed.handoff_hash == wire.hash()?
                    && if submit {
                        matches!(typed.stage, CheckStage::Admit | CheckStage::RemoteRead)
                    } else {
                        typed.stage == CheckStage::RemoteRead
                    },
                "Task business challenge changed its source, wire or stage"
            );
            let challenge_hash = challenge.hash()?;
            let receipts = {
                let store = self.store.lock().await;
                SignatureVerification::new().run(|| {
                    store.task_business_challenge_receipts(
                        &self.identity,
                        source.current_policy(),
                        TaskBusinessAuthorization::Submit(&authorization),
                        &business,
                        &header_hash,
                        challenge,
                        now_ms() as u64,
                    )
                })?
            };
            response = client
                .exchange(&TaskBusinessRequest::Receipts {
                    source_context: header_hash,
                    business_wire_hash: wire_hash,
                    challenge_hash,
                    receipts,
                })
                .await?;
        }
        match response {
            TaskBusinessResponse::Business(value) if submit => Ok(value),
            TaskBusinessResponse::Status(value) => Ok(value),
            _ => bail!("Task business submission has no final result for its original request"),
        }
    }

    async fn advance_execution_dispatch(
        self: &Arc<Self>,
        space: &str,
        operation_id: &str,
        allow_first_submit: bool,
    ) -> Result<()> {
        self.advance_execution_dispatch_with_verification(
            space,
            operation_id,
            allow_first_submit,
            &SignatureVerification::new(),
        )
        .await
    }

    async fn advance_execution_dispatch_with_verification(
        self: &Arc<Self>,
        space: &str,
        operation_id: &str,
        allow_first_submit: bool,
        verification: &SignatureVerification,
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
                verification.run(|| {
                    store.prepare_execution_dispatch_handoff(
                        &self.identity,
                        space,
                        operation_id,
                        now_ms() as u64,
                    )
                })?
            } else {
                let Some(wire) = store.execution_dispatch_handoff(space, operation_id)? else {
                    return Ok(());
                };
                (wire, false)
            };
            (wire, fresh, operation.request.target_node)
        };
        let result = self
            .request_execution_dispatch_with_verification(
                space,
                &target,
                &wire,
                fresh,
                verification,
            )
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
            verification.run(|| {
                store.apply_execution_handoff_receipt(&self.identity, &result, now_ms() as u64)
            })
        });
        match applied {
            Ok(operation) => {
                drop(store);
                self.publish_changed(space, &operation.request.conversation_id, "");
                if operation.state.terminal() {
                    self.complete_execution_dispatch(&operation, verification)
                        .await?;
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

    pub(super) fn resolve_execution_dispatch<'a>(
        self: &'a Arc<Self>,
        access: &'a Access,
    ) -> futures_util::future::BoxFuture<'a, Result<Value>> {
        Box::pin(async move {
            use vibeface_secure::authorization::execution_handoff::{
                ExecutionHandoffResolutionResult, ExecutionHandoffTerminalReconciliationResult,
                ExecutionResolutionPurpose,
            };
            ensure!(
                access.account_actor(),
                "remote execution resolution requires the user"
            );
            let (wire, completed) = {
                let mut store = self.store.lock().await;
                access.verification.run(|| {
                    let dispatch = store.execution_dispatch_facts_in_scope(
                        &access.authorization,
                        &access.body,
                        access.authorization.audience(),
                        now_ms() as u64,
                    )?;
                    let (wire, fresh) = store.prepare_execution_request_dispatch_resolution(
                        &self.identity,
                        &dispatch,
                        &access.authorization,
                        &access.freshness,
                        &access.transport,
                        &access.body,
                        now_ms() as u64,
                    )?;
                    let completed = if fresh {
                        None
                    } else {
                        completed_execution_resolution(&store, &wire)?
                    };
                    Ok::<_, anyhow::Error>((wire, completed))
                })?
            };
            let operation = if let Some(completed) = completed {
                completed
            } else {
                let response = self
                .request_execution_dispatch_resolution(&wire)
                .await
                .context("remote resolution outcome is unconfirmed; reconcile its original authorization before retrying")?;
                let mut store = self.store.lock().await;
                access.verification.run(|| match wire.resolution.purpose {
                    ExecutionResolutionPurpose::ResolveUnknown => {
                        let result: ExecutionHandoffResolutionResult =
                            serde_json::from_value(response)?;
                        ensure!(
                            result.resolution == wire,
                            "executor changed the admitted resolution"
                        );
                        if let Some(completed) = completed_execution_resolution(&store, &wire)? {
                            return Ok(completed);
                        }
                        store.apply_execution_dispatch_resolution(
                            &self.identity,
                            &result,
                            now_ms() as u64,
                        )
                    }
                    ExecutionResolutionPurpose::ReconcileTerminal => {
                        let result: ExecutionHandoffTerminalReconciliationResult =
                            serde_json::from_value(response)?;
                        ensure!(
                            result.reconciliation == wire,
                            "executor changed the admitted terminal reconciliation"
                        );
                        if let Some(completed) = completed_execution_resolution(&store, &wire)? {
                            return Ok(completed);
                        }
                        store.apply_execution_dispatch_terminal_reconciliation(
                            &self.identity,
                            &result,
                            now_ms() as u64,
                        )
                    }
                })?
            };
            self.publish_changed(&access.network_id, &operation.request.conversation_id, "");
            if operation.state.terminal() {
                self.complete_execution_dispatch(&operation, &access.verification)
                    .await?;
            }
            let store = self.store.lock().await;
            let operation = execution_operation_view(&store, &operation)?;
            Ok(json!({"execution_source":operation["execution_source"],"operation":operation}))
        })
    }

    pub(super) async fn remote_operation_status(
        &self,
        access: &Access,
        target: &str,
        operation_id: &str,
    ) -> Result<Value> {
        {
            let store = self.store.lock().await;
            access.verification.run(|| {
                store.authorize_request_read(
                    &access.authorization,
                    &access.freshness,
                    &access.transport,
                    now_ms() as u64,
                )
            })?;
            let operation = store
                .operation(&access.network_id, operation_id)?
                .context("remote operation is missing")?;
            ensure!(
                operation.request.target_node == target,
                "operation belongs to another execution target"
            );
        }
        let (operation, actual, error) = self
            .execution_dispatch_metadata(&access.network_id, operation_id, &access.verification)
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
            access.verification.run(|| {
                store.authorize_request_read(
                    &access.authorization,
                    &access.freshness,
                    &access.transport,
                    now_ms() as u64,
                )
            })?;
            let operation = store
                .operation(&access.network_id, operation_id)?
                .context("session has no authorized Project dispatch")?;
            ensure!(
                operation.request.target_node == target
                    && operation.request.session_id.as_deref() == Some(session)
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
            .execution_dispatch_metadata(&access.network_id, operation_id, &access.verification)
            .await?;
        let operation = execution_operation_view(&*self.store.lock().await, &operation)?;
        Ok(
            json!({"session":actual.as_ref().map(|result| &result.receipt.payload.session),
            "receipt":actual.as_ref().map(|result| &result.receipt),"operation":operation,
            "execution_source":operation["execution_source"],"remote_verified":actual.is_some(),"actual":actual,"refresh_error":error}),
        )
    }

    pub(super) async fn execution_dispatch_metadata(
        &self,
        space: &str,
        operation_id: &str,
        verification: &SignatureVerification,
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
            .request_execution_dispatch_with_verification(
                space,
                &operation.request.target_node,
                &wire,
                false,
                verification,
            )
            .await
        {
            Ok(actual) => Ok((operation, Some(actual), None)),
            Err(error) => Ok((operation, None, Some(error.to_string()))),
        }
    }
}
