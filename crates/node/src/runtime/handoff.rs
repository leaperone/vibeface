use super::*;
use crate::identity::execution_handoff::AdmittedExecutionHandoff;
use vibeface_secure::authorization::execution_handoff::{
    ExecutionHandoffChallenge, ExecutionHandoffFreshness, ExecutionHandoffResolutionChallenge,
    ExecutionHandoffResolutionFreshness, ExecutionHandoffResolutionResult, ExecutionHandoffResult,
    ExecutionHandoffTerminalReconciliationResult, ExecutionResolutionPurpose,
    SignedExecutionHandoff, SignedExecutionHandoffResolution,
};

fn original(wire: &SignedExecutionHandoff) -> Result<(ResourceRef, String)> {
    let event: Event = serde_json::from_str(&wire.handoff.project_dispatch.canonical_body)?;
    let EventBody::DispatchRecorded { operation_id, .. } = event.body else {
        bail!("execution handoff has no Project dispatch")
    };
    Ok((
        ResourceRef {
            space: SpaceId::parse(&event.network_id)?,
            resource: event.resource_id,
        },
        operation_id,
    ))
}

fn context(value: &AdmittedExecutionHandoff) -> ExecutionContext {
    ExecutionContext {
        space_id: value.execution_context.space_id.clone(),
        resource_id: value.execution_context.resource_id.clone(),
        operation_id: value.original_request.operation_id.clone(),
        shared: true,
        lease_expires_at_ms: value.execution_context.lease_expires_at_ms.unwrap_or(0),
    }
}

fn turn_key(context: &ExecutionContext) -> String {
    format!(
        "handoff-turn:{}",
        crypto::digest(
            &serde_json::to_vec(&(
                &context.space_id,
                &context.resource_id,
                &context.operation_id,
            ))
            .expect("execution identifiers serialize")
        )
    )
}

fn public_result(
    wire: SignedExecutionHandoff,
    value: AdmittedExecutionHandoff,
) -> Result<ExecutionHandoffResult> {
    Ok(ExecutionHandoffResult {
        handoff: wire,
        receipt: value.receipt.context("executor did not sign its state")?,
        result: value.result,
    })
}

fn provider_result(mut value: Value, native: Option<&str>) -> Value {
    fn clean(value: &mut Value, native: Option<&str>) {
        match value {
            Value::Object(fields) => {
                for key in ["native_session_id", "threadId", "thread_id"] {
                    fields.remove(key);
                }
                for value in fields.values_mut() {
                    clean(value, native);
                }
            }
            Value::Array(values) => {
                for value in values {
                    clean(value, native);
                }
            }
            Value::String(text) if native.is_some_and(|native| native == text) => {
                *value = Value::Null
            }
            Value::String(text) => {
                if let Some(native) = native.filter(|native| !native.is_empty()) {
                    *text = text.replace(native, "[private session]");
                }
            }
            _ => {}
        }
    }
    clean(&mut value, native);
    value
}

impl Runtime {
    pub(super) async fn complete_execution_dispatch(
        self: &Arc<Self>,
        operation: &OperationRecord,
    ) -> Result<()> {
        ensure!(
            operation.state.terminal()
                && operation.request.target_node != self.identity.node_id()?,
            "delegated completion requires an already committed remote terminal receipt"
        );
        {
            let store = self.store.lock().await;
            ensure!(
                store
                    .operation(
                        &operation.request.network_id,
                        &operation.request.operation_id
                    )?
                    .as_ref()
                    == Some(operation),
                "delegated completion changed after its receipt was committed"
            );
            let handoff = if operation.result.get("resolution").is_some() {
                let result: ExecutionHandoffResolutionResult =
                    serde_json::from_value(operation.result.clone())?;
                result.resolution.resolution.observation.handoff
            } else if operation.result.get("reconciliation").is_some() {
                let result: ExecutionHandoffTerminalReconciliationResult =
                    serde_json::from_value(operation.result.clone())?;
                result.reconciliation.resolution.observation.handoff
            } else {
                let result: ExecutionHandoffResult =
                    serde_json::from_value(operation.result.clone())?;
                result.handoff
            };
            ensure!(
                store
                    .execution_dispatch_handoff(
                        &operation.request.network_id,
                        &operation.request.operation_id
                    )?
                    .as_ref()
                    == Some(&handoff),
                "delegated completion does not match its durable carrier"
            );
        }
        self.finish(
            &operation.request,
            operation.state.clone(),
            operation.result.clone(),
        )
        .await
    }

    async fn collect_handoff_receipts(
        &self,
        route: &SpaceId,
        policies: &[vibeface_secure::authorization::PolicyStamp],
        mut receipts: Vec<Signed<FreshnessReceipt>>,
        kind: &str,
        challenge: Value,
    ) -> Result<Vec<Signed<FreshnessReceipt>>> {
        let targets = {
            let store = self.store.lock().await;
            let mut targets = BTreeSet::new();
            for stamp in policies {
                if receipts
                    .iter()
                    .any(|receipt| receipt.payload.current.domain == stamp.domain)
                {
                    continue;
                }
                let authority = match &stamp.domain {
                    PolicyDomain::Account(account) => {
                        &store
                            .authorization_state()
                            .account(account)
                            .context("handoff account authority is missing")?
                            .authority
                    }
                    PolicyDomain::Space(space) => {
                        &store
                            .authorization_state()
                            .space(space)
                            .context("handoff space authority is missing")?
                            .authority
                    }
                };
                targets.insert(crypto::fingerprint(authority.public_key.as_str())?);
            }
            targets
        };
        for target in targets {
            let reply = self
                .peer_request(
                    route.as_str(),
                    &target,
                    json!({"type":kind,"challenge":challenge}),
                )
                .await?;
            let signed: Vec<Signed<FreshnessReceipt>> =
                serde_json::from_value(reply["receipts"].clone())?;
            receipts.extend(signed);
        }
        receipts.sort_by(|left, right| {
            left.payload
                .current
                .domain
                .cmp(&right.payload.current.domain)
        });
        ensure!(
            receipts.len() == policies.len(),
            "handoff policy authority is unavailable"
        );
        Ok(receipts)
    }

    async fn handoff_freshness(
        &self,
        wire: &SignedExecutionHandoff,
        transport: &TransportEvidence,
        stage: CheckStage,
    ) -> Result<ExecutionHandoffFreshness> {
        let (challenge, receipts) = {
            let mut store = self.store.lock().await;
            let now = now_ms() as u64;
            let challenge = store.issue_execution_handoff_challenge(wire, transport, stage, now)?;
            let receipts =
                store.sign_execution_handoff_receipts(&challenge, &self.identity, now)?;
            (challenge, receipts)
        };
        let (project, _) = original(wire)?;
        let receipts = self
            .collect_handoff_receipts(
                &project.space,
                &challenge.policies,
                receipts,
                "execution_handoff_receipts",
                json!(challenge),
            )
            .await?;
        Ok(ExecutionHandoffFreshness {
            challenge,
            receipts,
        })
    }

    async fn handoff_resolution_freshness(
        &self,
        wire: &SignedExecutionHandoffResolution,
        transport: &TransportEvidence,
    ) -> Result<ExecutionHandoffResolutionFreshness> {
        let (challenge, receipts) = {
            let mut store = self.store.lock().await;
            let now = now_ms() as u64;
            let challenge =
                store.issue_execution_handoff_resolution_challenge(wire, transport, now)?;
            let receipts = store.sign_execution_handoff_resolution_receipts(
                &challenge,
                &self.identity,
                now,
            )?;
            (challenge, receipts)
        };
        let (project, _) = original(&wire.resolution.observation.handoff)?;
        let receipts = self
            .collect_handoff_receipts(
                &project.space,
                &challenge.policies,
                receipts,
                "execution_handoff_resolution_receipts",
                json!(challenge),
            )
            .await?;
        Ok(ExecutionHandoffResolutionFreshness {
            challenge,
            receipts,
        })
    }

    async fn resolve_handoff_peer(
        self: &Arc<Self>,
        message: &Inbound,
        origin: DeviceId,
    ) -> Result<Value> {
        let _work = self.maintenance.enter()?;
        let wire: SignedExecutionHandoffResolution =
            serde_json::from_value(message.payload["resolution"].clone())?;
        ensure!(
            wire.invocation.payload.device == origin && wire.invocation.payload.presenter == origin,
            "resolution presenter differs from the authenticated peer"
        );
        let handoff = &wire.resolution.observation.handoff;
        ensure!(
            handoff.handoff.execution_audience == self.audience()?
                && handoff.handoff.target_public_key.as_str() == self.identity.public_key_hex()?,
            "resolution reached another executor"
        );
        let (project, operation) = original(handoff)?;
        ensure!(
            project.space.as_str() == message.network_id,
            "resolution route differs from its original Project"
        );
        let transport = TransportEvidence::VerifiedPeer { origin };
        let _resolution = self.handoff_resolution_lock.lock().await;
        {
            let store = self.store.lock().await;
            if let Some(result) =
                store.execution_handoff_resolution_status(&wire, &transport, now_ms() as u64)?
            {
                return Ok(json!(result));
            }
            store.execution_handoff_resolution_authorization(&wire, &transport, now_ms() as u64)?;
        }
        if wire.resolution.purpose == ExecutionResolutionPurpose::ReconcileTerminal {
            let proof = self.handoff_resolution_freshness(&wire, &transport).await?;
            let store = self.store.lock().await;
            let auth = store.execution_handoff_resolution_authorization(
                &wire,
                &transport,
                now_ms() as u64,
            )?;
            return Ok(json!(
                store.authorize_execution_handoff_terminal_reconciliation(
                    &auth,
                    &proof,
                    now_ms() as u64,
                )?
            ));
        }
        let proof = self.handoff_resolution_freshness(&wire, &transport).await?;
        {
            let store = self.store.lock().await;
            let auth = store.execution_handoff_resolution_authorization(
                &wire,
                &transport,
                now_ms() as u64,
            )?;
            store.authorize_execution_handoff_resolution_fence(&auth, &proof, now_ms() as u64)?;
        }
        let context = ExecutionContext {
            space_id: project.space,
            resource_id: project.resource,
            operation_id: operation,
            shared: true,
            lease_expires_at_ms: proof.challenge.expires_at_ms,
        };
        let mut affected = self.engine.affected_operations(&context).await;
        let stopped = self.engine.fence_operation(&context).await;
        if let Ok((_, operations)) = &stopped {
            affected.extend(operations.iter().cloned());
        }
        self.mark_fenced_operations(
            &context,
            affected,
            "execution stopped for an explicit unknown-result resolution",
            stopped.is_ok(),
        )
        .await?;
        let (stopped, _) = stopped?;
        self.engine.complete_fenced_operation(&stopped).await?;
        let proof = self.handoff_resolution_freshness(&wire, &transport).await?;
        Ok(json!(self.store.lock().await.resolve_execution_handoff(
            &self.identity,
            &wire,
            &proof,
            &transport,
            &stopped,
            now_ms() as u64,
        )?))
    }

    pub(super) async fn handle_handoff_peer(
        self: &Arc<Self>,
        message: &Inbound,
        origin: DeviceId,
    ) -> Result<Value> {
        let envelope = &message.payload;
        if envelope["type"] == "execution_handoff_resolve" {
            return self.resolve_handoff_peer(message, origin).await;
        }
        if envelope["type"] == "execution_handoff_resolution_receipts" {
            let challenge: ExecutionHandoffResolutionChallenge =
                serde_json::from_value(envelope["challenge"].clone())?;
            return Ok(json!({"receipts":self.store.lock().await
                .sign_execution_handoff_resolution_receipts(
                    &challenge, &self.identity, now_ms() as u64,
                )?}));
        }
        if envelope["type"] == "execution_handoff_receipts" {
            let challenge: ExecutionHandoffChallenge =
                serde_json::from_value(envelope["challenge"].clone())?;
            return Ok(
                json!({"receipts":self.store.lock().await.sign_execution_handoff_receipts(&challenge, &self.identity, now_ms() as u64)?}),
            );
        }
        let _work = self.maintenance.enter()?;
        let wire: SignedExecutionHandoff = serde_json::from_value(envelope["handoff"].clone())?;
        ensure!(
            wire.invocation.payload.device == origin && wire.invocation.payload.presenter == origin,
            "handoff presenter differs from the authenticated peer"
        );
        ensure!(
            wire.handoff.execution_audience == self.audience()?
                && wire.handoff.target_public_key.as_str() == self.identity.public_key_hex()?,
            "handoff reached another executor"
        );
        let (project, operation) = original(&wire)?;
        ensure!(
            project.space.as_str() == message.network_id,
            "handoff route differs from its original Project"
        );
        let execution = wire.handoff.execution_resource.space.as_str();
        let transport = TransportEvidence::VerifiedPeer { origin };
        let existing = self
            .store
            .lock()
            .await
            .find_execution_handoff_by_original(&project, &operation)?
            .is_some();
        let value = if existing || envelope["type"] == "execution_handoff_status" {
            let proof = self
                .handoff_freshness(&wire, &transport, CheckStage::RemoteRead)
                .await?;
            let store = self.store.lock().await;
            let admitted = store.execution_handoff_authorization(
                execution,
                project.space.as_str(),
                &operation,
                now_ms() as u64,
            )?;
            ensure!(
                admitted.wire() == &wire,
                "handoff status changed its original signed request"
            );
            store.authorize_execution_handoff_read(&admitted, &proof, now_ms() as u64)?;
            store.execution_handoff_status(
                &self.identity,
                execution,
                project.space.as_str(),
                &operation,
                now_ms() as u64,
            )?
        } else {
            ensure!(
                envelope["type"] == "execution_handoff_submit",
                "unsupported execution handoff operation"
            );
            let proof = self
                .handoff_freshness(&wire, &transport, CheckStage::Admit)
                .await?;
            self.store.lock().await.accept_execution_handoff(
                &self.identity,
                &wire,
                &proof,
                &transport,
                now_ms() as u64,
            )?
        };
        if envelope["type"] == "execution_handoff_submit" && value.state == OperationState::Accepted
        {
            let node = self.clone();
            let value = value.clone();
            tokio::spawn(async move {
                if let Err(error) = node.execute_handoff(value).await {
                    tracing::warn!(%error, "accepted execution handoff could not start");
                }
            });
        }
        Ok(json!(public_result(wire, value)?))
    }

    async fn finish_handoff(
        self: &Arc<Self>,
        value: &AdmittedExecutionHandoff,
        mut state: OperationState,
        mut result: Value,
    ) -> Result<()> {
        let _completion = self.handoff_resolution_lock.lock().await;
        let current = self.store.lock().await.execution_handoff_status(
            &self.identity,
            value.execution_context.execution_space_id.as_str(),
            value.execution_context.space_id.as_str(),
            &value.original_request.operation_id,
            now_ms() as u64,
        )?;
        if current.state.terminal() || current.state == OperationState::Unknown {
            return Ok(());
        }
        let context = context(&current);
        let mut cleanup_affected = Vec::new();
        if state.terminal() && current.state == OperationState::Running {
            let affected = self.engine.affected_operations(&context).await;
            if let Err(error) = self.engine.complete_operation(&context).await {
                state = OperationState::Unknown;
                result = json!({
                    "error":format!("execution cleanup could not be confirmed: {error:#}"),
                    "observed_result":result,
                });
                cleanup_affected = affected;
            }
        }
        self.persist_handoff_result(&current, state.clone(), result)
            .await?;
        if state == OperationState::Unknown {
            let mut affected = cleanup_affected;
            affected.extend(self.engine.affected_operations(&context).await);
            let stopped = self.engine.fence_operation(&context).await;
            if let Ok((_, operations)) = &stopped {
                affected.extend(operations.iter().cloned());
            }
            self.mark_fenced_operations(
                &context,
                affected,
                "execution outcome requires reconciliation",
                stopped.is_ok(),
            )
            .await?;
            stopped?;
        }
        Ok(())
    }

    async fn persist_handoff_result(
        &self,
        value: &AdmittedExecutionHandoff,
        state: OperationState,
        result: Value,
    ) -> Result<()> {
        let project = ResourceRef {
            space: value.execution_context.space_id.clone(),
            resource: value.execution_context.resource_id.clone(),
        };
        let mut store = self.store.lock().await;
        let session = store.execution_handoff_session(
            value.execution_context.execution_space_id.as_str(),
            &project,
            &value.original_request.target_node,
            &value.original_request.session_id,
        )?;
        store.finish_execution_handoff(
            &self.identity,
            value.execution_context.execution_space_id.as_str(),
            project.space.as_str(),
            &value.original_request.operation_id,
            state,
            result,
            session.as_ref(),
            now_ms() as u64,
        )?;
        Ok(())
    }

    async fn execute_handoff(self: Arc<Self>, accepted: AdmittedExecutionHandoff) -> Result<()> {
        let _work = self.maintenance.enter()?;
        let preparation = async {
            ensure!(
                self.config.roles.contains(&NodeRole::Executor),
                "executor capability is disabled"
            );
            let auth = self.store.lock().await.execution_handoff_authorization(
                accepted.execution_context.execution_space_id.as_str(),
                accepted.execution_context.space_id.as_str(),
                &accepted.original_request.operation_id,
                now_ms() as u64,
            )?;
            let transport = TransportEvidence::VerifiedPeer {
                origin: auth.wire().invocation.payload.device.clone(),
            };
            let proof = self
                .handoff_freshness(auth.wire(), &transport, CheckStage::Start)
                .await?;
            let session = accepted
                .execution_session
                .as_ref()
                .context("accepted handoff has no local session")?;
            let kind: ExecutorKind = serde_json::from_value(json!(session.executor))?;
            let action = self.engine_action(&accepted.original_request.action)?;
            self.allowed_path(&session.cwd)?;
            for resource in &accepted.original_request.resources {
                self.allowed_path(resource)?;
            }
            let (value, fresh, _) = self.store.lock().await.claim_execution_handoff(
                &self.identity,
                &auth,
                &proof,
                now_ms() as u64,
            )?;
            Ok::<_, anyhow::Error>((value, fresh, kind, action))
        }
        .await;
        let (value, fresh, kind, action) = match preparation {
            Ok(value) => value,
            Err(error) => {
                let current = self.store.lock().await.execution_handoff_status(
                    &self.identity,
                    accepted.execution_context.execution_space_id.as_str(),
                    accepted.execution_context.space_id.as_str(),
                    &accepted.original_request.operation_id,
                    now_ms() as u64,
                )?;
                if current.state == OperationState::Accepted {
                    self.finish_handoff(
                        &current,
                        OperationState::Rejected,
                        json!({"error":format!("{error:#}")}),
                    )
                    .await?;
                }
                return Err(error);
            }
        };
        if !fresh {
            return Ok(());
        }
        let context = context(&value);
        let session = value
            .execution_session
            .as_ref()
            .context("claimed handoff lost its local session")?;
        let native = ExecutorSession {
            executor: kind,
            cwd: PathBuf::from(&session.cwd),
            native_session_id: session.native_session_id.clone(),
        };
        self.supervise_handoff(value.clone()).await;
        let store = self.store.clone();
        let execution = value.execution_context.execution_space_id.clone();
        let project = ResourceRef {
            space: context.space_id.clone(),
            resource: context.resource_id.clone(),
        };
        let operation = context.operation_id.clone();
        let result = self
            .engine
            .execute_authorized(&context, &native, action.clone(), move |id| async move {
                store.lock().await.persist_execution_handoff_native(
                    execution.as_str(),
                    &project,
                    &operation,
                    &id,
                    now_ms() as u64,
                )?;
                Ok(())
            })
            .await;
        match result {
            Ok(result)
                if native.executor == ExecutorKind::Codex
                    && matches!(action, EngineAction::Send { .. }) =>
            {
                self.store.lock().await.local_set(
                    value.execution_context.execution_space_id.as_str(),
                    &turn_key(&context),
                    &result.result,
                )?;
                if let (Some(native), Some(turn)) = (
                    result.native_session_id.as_deref(),
                    result.result["turn"]["id"].as_str(),
                ) {
                    if let Ok(Some(observed)) = self
                        .engine
                        .read_turn_authorized(&context, native, turn)
                        .await
                    {
                        if matches!(
                            observed["status"].as_str(),
                            Some("completed" | "failed" | "interrupted")
                        ) {
                            let state = if observed["status"] == "completed" {
                                OperationState::Succeeded
                            } else {
                                OperationState::Failed
                            };
                            self.finish_handoff(
                                &value,
                                state,
                                provider_result(observed, Some(native)),
                            )
                            .await?;
                        }
                    }
                }
            }
            Ok(result) => {
                self.finish_handoff(
                    &value,
                    OperationState::Succeeded,
                    provider_result(result.result, result.native_session_id.as_deref()),
                )
                .await?
            }
            Err(error) => {
                self.finish_handoff(
                    &value,
                    if error.uncertain {
                        OperationState::Unknown
                    } else {
                        OperationState::Failed
                    },
                    provider_result(
                        json!({"error":error.message}),
                        error.native_session_id.as_deref(),
                    ),
                )
                .await?
            }
        }
        Ok(())
    }

    async fn supervise_handoff(self: &Arc<Self>, value: AdmittedExecutionHandoff) {
        let mut context = context(&value);
        let key = (context.space_id.to_string(), context.operation_id.clone());
        if !self.execution_leases.lock().await.insert(key.clone()) {
            return;
        }
        let node = self.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(5)).await;
                let status = node.store.lock().await.execution_handoff_status(
                    &node.identity,
                    value.execution_context.execution_space_id.as_str(),
                    context.space_id.as_str(),
                    &context.operation_id,
                    now_ms() as u64,
                );
                match status {
                    Ok(current) if current.state.terminal() => break,
                    Ok(current) if current.state == OperationState::Running => {}
                    _ => {
                        let _ = node
                            .stop_handoff(&value, "execution outcome requires reconciliation")
                            .await;
                        break;
                    }
                }
                let renewal = async {
                    let auth = node.store.lock().await.execution_handoff_authorization(
                        value.execution_context.execution_space_id.as_str(),
                        context.space_id.as_str(),
                        &context.operation_id,
                        now_ms() as u64,
                    )?;
                    let transport = TransportEvidence::VerifiedPeer {
                        origin: auth.wire().invocation.payload.device.clone(),
                    };
                    let proof = node
                        .handoff_freshness(auth.wire(), &transport, CheckStage::Renew)
                        .await?;
                    let deadline = node.store.lock().await.renew_execution_handoff(
                        &auth,
                        &proof,
                        now_ms() as u64,
                    )?;
                    node.engine.renew_operation(&context, deadline).await?;
                    Ok::<_, anyhow::Error>(deadline)
                }
                .await;
                match renewal {
                    Ok(deadline) => context.lease_expires_at_ms = deadline,
                    Err(error) => {
                        let _ = node
                            .stop_handoff(
                                &value,
                                &format!("authorization lease could not renew: {error:#}"),
                            )
                            .await;
                        break;
                    }
                }
            }
            node.execution_leases.lock().await.remove(&key);
        });
    }

    async fn stop_handoff(
        self: &Arc<Self>,
        value: &AdmittedExecutionHandoff,
        reason: &str,
    ) -> Result<()> {
        let _completion = self.handoff_resolution_lock.lock().await;
        let context = context(value);
        let mut affected = self.engine.affected_operations(&context).await;
        let stopped = self.engine.fence_operation(&context).await;
        if let Ok((_, operations)) = &stopped {
            affected.extend(operations.iter().cloned());
        }
        affected.push(context.operation_id.clone());
        self.mark_fenced_operations(&context, affected, reason, stopped.is_ok())
            .await?;
        stopped.map(|_| ())
    }

    async fn mark_fenced_operations(
        self: &Arc<Self>,
        context: &ExecutionContext,
        mut affected: Vec<String>,
        reason: &str,
        confirmed: bool,
    ) -> Result<()> {
        let project = ResourceRef {
            space: context.space_id.clone(),
            resource: context.resource_id.clone(),
        };
        affected.sort();
        affected.dedup();
        for operation in affected {
            let scope = self
                .store
                .lock()
                .await
                .find_execution_handoff_by_original(&project, &operation)?;
            if let Some(scope) = scope {
                let current = self.store.lock().await.execution_handoff_status(
                    &self.identity,
                    scope.execution_space_id.as_str(),
                    project.space.as_str(),
                    &operation,
                    now_ms() as u64,
                )?;
                if current.state == OperationState::Running {
                    self.persist_handoff_result(
                        &current,
                        OperationState::Unknown,
                        json!({"error":reason,"cancellation_confirmed":confirmed}),
                    )
                    .await?;
                }
                continue;
            }
            let direct = {
                let store = self.store.lock().await;
                let direct = store.operation(project.space.as_str(), &operation)?;
                if let Some(direct) = &direct {
                    if direct.state != OperationState::Running
                        || direct.request.target_node != self.identity.node_id()?
                    {
                        continue;
                    }
                    let actual = execution_context(&store, &direct.request, 0)?;
                    if !actual.shared
                        || actual.space_id != context.space_id
                        || actual.resource_id != context.resource_id
                        || actual.operation_id != operation
                    {
                        continue;
                    }
                }
                direct
            };
            if let Some(direct) = direct {
                self.finish(
                    &direct.request,
                    OperationState::Unknown,
                    json!({"error":reason,"cancellation_confirmed":confirmed}),
                )
                .await?;
            }
        }
        Ok(())
    }

    pub(super) async fn observe_handoff(
        self: &Arc<Self>,
        envelope: &Value,
        project: ResourceRef,
        operation: &str,
    ) -> Result<()> {
        let scope = self
            .store
            .lock()
            .await
            .find_execution_handoff_by_original(&project, operation)?;
        let Some(scope) = scope else {
            return Ok(());
        };
        let value = self.store.lock().await.execution_handoff_status(
            &self.identity,
            scope.execution_space_id.as_str(),
            project.space.as_str(),
            operation,
            now_ms() as u64,
        )?;
        if value.state != OperationState::Running || value.original_request.action["type"] != "send"
        {
            return Ok(());
        }
        ensure!(
            envelope["execution_context"]["container_id"]
                .as_str()
                .is_some_and(|id| id.len() == 64),
            "handoff event has no isolated container identity"
        );
        let event = &envelope["event"];
        if event["method"] == "bridge.codex_disconnected" {
            return self
                .stop_handoff(&value, "executor disconnected; reconcile before retrying")
                .await;
        }
        let session = value
            .execution_session
            .as_ref()
            .context("running handoff lost its local session")?;
        let native = event["params"]["threadId"]
            .as_str()
            .or_else(|| event["params"]["thread_id"].as_str())
            .or_else(|| event["native_session_id"].as_str());
        if native.is_none() || native != session.native_session_id.as_deref() {
            return Ok(());
        }
        if event["method"] == "turn/completed" {
            let turn = field(&event["params"]["turn"], "id")?;
            let known = self.store.lock().await.local_value(
                scope.execution_space_id.as_str(),
                &turn_key(&context(&value)),
            )?;
            if known
                .as_ref()
                .and_then(|value| value["turn"]["id"].as_str())
                != Some(turn)
            {
                return Ok(());
            }
            let state = if event["params"]["turn"]["status"] == "completed" {
                OperationState::Succeeded
            } else {
                OperationState::Failed
            };
            self.finish_handoff(
                &value,
                state,
                provider_result(event["params"]["turn"].clone(), native),
            )
            .await?;
        }
        Ok(())
    }

    pub(super) async fn recover_handoffs(self: &Arc<Self>) -> Result<()> {
        let values = self
            .store
            .lock()
            .await
            .load_execution_handoffs_for_recovery(&self.identity, now_ms() as u64)?;
        for value in values {
            if value.state == OperationState::Unknown {
                if let Err(error) = self
                    .stop_handoff(&value, "unresolved execution after restart")
                    .await
                {
                    tracing::warn!(%error, "unresolved handoff could not be fenced after restart");
                }
                continue;
            }
            if value.state != OperationState::Accepted {
                continue;
            }
            let node = self.clone();
            tokio::spawn(async move {
                if let Err(error) = node.execute_handoff(value).await {
                    tracing::warn!(%error, "accepted handoff recovery failed");
                }
            });
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn public_provider_result_preserves_output_without_native_handles() {
        let result = provider_result(
            json!({
                "thread":{"id":"private-native","output":"requested output"},
                "native_session_id":"private-native",
                "events":[{"threadId":"private-native","thread_id":"private-native","text":"visible result"}],
            }),
            Some("private-native"),
        );
        assert_eq!(
            result,
            json!({"thread":{"id":null,"output":"requested output"},"events":[{"text":"visible result"}]})
        );
        assert!(
            !serde_json::to_string(&result)
                .unwrap()
                .contains("private-native")
        );
    }
}
