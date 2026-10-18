use super::*;
use crate::identity::execution_handoff::AdmittedExecutionHandoff;
use crate::identity::task_business_policy::{
    TaskBusinessPolicySession, TaskBusinessPolicySourceContext, TaskBusinessRequest,
    TaskBusinessResponse,
};
use vibeface_secure::authorization::execution_handoff::{
    ExecutionHandoffChallenge, ExecutionHandoffFreshness, ExecutionHandoffResolutionChallenge,
    ExecutionHandoffResolutionFreshness, ExecutionHandoffResolutionResult, ExecutionHandoffResult,
    ExecutionHandoffTerminalReconciliationResult, ExecutionResolutionPurpose,
    SignedExecutionHandoff, SignedExecutionHandoffResolution,
};
use vibeface_secure::authorization::{
    remote_execution_history::RemoteExecutionHistoryAcceptance,
    task_business_policy::{
        TaskBusinessAuthorization, TaskBusinessWire, VerifiedTaskBusinessOriginalSource,
        original_operation::VerifiedTaskBusinessOperationCurrent,
        receipt::{TaskBusinessChallenge, TaskBusinessReceiptRead},
    },
};
use vibeface_secure::signature_verification::SignatureVerification;

#[derive(Clone)]
#[allow(clippy::large_enum_variant)]
enum HandoffExecutionPolicy {
    Primary,
    TaskBusiness {
        source: VerifiedTaskBusinessOriginalSource,
        acceptance: RemoteExecutionHistoryAcceptance,
        current: Option<VerifiedTaskBusinessOperationCurrent>,
    },
}

#[allow(clippy::large_enum_variant)]
enum HandoffExecutionCheck {
    Primary {
        authorization: vibeface_secure::authorization::execution_handoff::VerifiedExecutionHandoff,
        freshness: ExecutionHandoffFreshness,
    },
    TaskBusiness(VerifiedTaskBusinessOperationCurrent),
}

type SharedHandoffExecutionPolicy = Arc<Mutex<HandoffExecutionPolicy>>;

enum HandoffProgram {
    Command(crate::command::CommandSpec),
    Session {
        kind: ExecutorKind,
        action: EngineAction,
    },
}

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

pub(super) fn handoff_is_command(wire: &SignedExecutionHandoff) -> Result<bool> {
    let event: Event = serde_json::from_str(&wire.handoff.project_dispatch.canonical_body)?;
    let EventBody::DispatchRecorded { action, .. } = event.body else {
        bail!("execution handoff has no Project dispatch")
    };
    Ok(action["action"]["type"] == "command")
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
    async fn handoff_affected(&self, value: &AdmittedExecutionHandoff) -> Vec<String> {
        if value.original_request.action["type"] == "command" {
            vec![value.original_request.operation_id.clone()]
        } else {
            self.engine.affected_operations(&context(value)).await
        }
    }

    pub(super) async fn fence_handoff(
        &self,
        value: &AdmittedExecutionHandoff,
    ) -> Result<(crate::engine::StoppedExecution, Vec<String>)> {
        if value.original_request.action["type"] == "command" {
            self.engine.fence_command(&context(value)).await
        } else {
            self.engine.fence_operation(&context(value)).await
        }
    }

    pub(super) async fn request_execution_dispatch_resolution(
        &self,
        wire: &SignedExecutionHandoffResolution,
    ) -> Result<Value> {
        let source = {
            let mut store = self.store.lock().await;
            SignatureVerification::new().run(|| -> Result<_> {
                if !store.execution_dispatch_uses_task_policy(
                    &self.identity,
                    &wire.resolution.observation.handoff,
                )? {
                    return Ok(None);
                }
                let (current, authorization) =
                    store.current_task_business_resolve(&self.identity, wire, now_ms() as u64)?;
                Ok(Some(store.begin_task_business_source(
                    &self.identity,
                    &current,
                    TaskBusinessAuthorization::Resolve(&authorization),
                    &TaskBusinessWire::Resolve(wire.clone()),
                    now_ms() as u64,
                )?))
            })?
        };
        let result = if let Some(source) = source {
            self.request_task_readonly_business(source, &TaskBusinessWire::Resolve(wire.clone()))
                .await
        } else {
            let (project, _) = original(&wire.resolution.observation.handoff)?;
            let target = crypto::fingerprint(
                wire.resolution
                    .observation
                    .handoff
                    .handoff
                    .target_public_key
                    .as_str(),
            )?;
            self.peer_request(
                project.space.as_str(),
                &target,
                json!({"type":"execution_handoff_resolve","resolution":wire}),
            )
            .await
        };
        result.context(
            "remote resolution outcome is unconfirmed; reconcile its original authorization before retrying",
        )
    }

    pub(super) async fn request_task_readonly_business(
        &self,
        mut source: TaskBusinessPolicySourceContext,
        wire: &TaskBusinessWire,
    ) -> Result<Value> {
        ensure!(
            matches!(
                wire,
                TaskBusinessWire::Observe(_) | TaskBusinessWire::Resolve(_)
            ),
            "readonly Task business requires an observation or resolution"
        );
        let source_context = source.header().hash()?;
        let business_wire_hash = wire.hash()?;
        let mut client = self.open_task_business_client(&mut source).await?;
        let mut response = if matches!(wire, TaskBusinessWire::Resolve(_)) {
            match client
                .exchange(&TaskBusinessRequest::Status { wire: wire.clone() })
                .await?
            {
                TaskBusinessResponse::Status(value) if value == json!({"state":"not_found"}) => {
                    client
                        .exchange(&TaskBusinessRequest::Business(wire.clone()))
                        .await?
                }
                TaskBusinessResponse::Status(value) => TaskBusinessResponse::Status(value),
                _ => bail!("resolution reconciliation returned an unexpected response"),
            }
        } else {
            client
                .exchange(&TaskBusinessRequest::Business(wire.clone()))
                .await?
        };
        ensure!(
            matches!(
                (wire, &response),
                (
                    TaskBusinessWire::Observe(_),
                    TaskBusinessResponse::Challenge {
                        challenge: TaskBusinessChallenge::Observe(_),
                        ..
                    }
                ) | (
                    TaskBusinessWire::Resolve(_),
                    TaskBusinessResponse::Challenge {
                        challenge: TaskBusinessChallenge::Resolve(_),
                        ..
                    }
                ) | (
                    TaskBusinessWire::Resolve(_),
                    TaskBusinessResponse::Status(_)
                )
            ),
            "Task business response skipped its required challenge"
        );
        if let TaskBusinessResponse::Challenge {
            source_context: returned_source,
            business_wire_hash: returned_wire,
            challenge,
        } = &response
        {
            ensure!(
                *returned_source == source_context && *returned_wire == business_wire_hash,
                "readonly Task business challenge changed its original source or wire"
            );
            let challenge_hash = challenge.hash()?;
            let receipts = {
                let store = self.store.lock().await;
                SignatureVerification::new().run(|| match (wire, challenge) {
                    (TaskBusinessWire::Observe(original), TaskBusinessChallenge::Observe(_)) => {
                        let (current, authorization) = store.current_task_business_observe(
                            &self.identity,
                            original,
                            now_ms() as u64,
                        )?;
                        store.task_business_challenge_receipts(
                            &self.identity,
                            &current,
                            TaskBusinessAuthorization::Observe(&authorization),
                            wire,
                            &source_context,
                            challenge,
                            now_ms() as u64,
                        )
                    }
                    (TaskBusinessWire::Resolve(original), TaskBusinessChallenge::Resolve(_)) => {
                        let (current, authorization) = store.current_task_business_resolve(
                            &self.identity,
                            original,
                            now_ms() as u64,
                        )?;
                        store.task_business_challenge_receipts(
                            &self.identity,
                            &current,
                            TaskBusinessAuthorization::Resolve(&authorization),
                            wire,
                            &source_context,
                            challenge,
                            now_ms() as u64,
                        )
                    }
                    _ => bail!("readonly Task business returned another challenge purpose"),
                })?
            };
            response = client
                .exchange(&TaskBusinessRequest::Receipts {
                    source_context,
                    business_wire_hash,
                    challenge_hash,
                    receipts,
                })
                .await?;
        }
        match (wire, response) {
            (TaskBusinessWire::Observe(_), TaskBusinessResponse::Status(value))
            | (TaskBusinessWire::Resolve(_), TaskBusinessResponse::Business(value))
            | (TaskBusinessWire::Resolve(_), TaskBusinessResponse::Status(value)) => Ok(value),
            _ => bail!("readonly Task business has no final result for its original request"),
        }
    }

    async fn handoff_execution_policy(
        &self,
        value: &AdmittedExecutionHandoff,
    ) -> Result<SharedHandoffExecutionPolicy> {
        let policy = match self
            .load_task_business_execution_original(
                value.execution_context.execution_space_id.as_str(),
                value.execution_context.space_id.as_str(),
                &value.original_request.operation_id,
            )
            .await?
        {
            Some((source, acceptance)) => HandoffExecutionPolicy::TaskBusiness {
                source,
                acceptance,
                current: None,
            },
            None => HandoffExecutionPolicy::Primary,
        };
        Ok(Arc::new(Mutex::new(policy)))
    }

    fn check_handoff_execution_policy<'a>(
        &'a self,
        value: &'a AdmittedExecutionHandoff,
        policy: &'a HandoffExecutionPolicy,
        stage: CheckStage,
        verification: &'a SignatureVerification,
    ) -> futures_util::future::BoxFuture<'a, Result<HandoffExecutionCheck>> {
        match policy {
            HandoffExecutionPolicy::Primary => Box::pin(async move {
                let authorization = {
                    let store = self.store.lock().await;
                    verification.run(|| {
                        store.execution_handoff_authorization(
                            value.execution_context.execution_space_id.as_str(),
                            value.execution_context.space_id.as_str(),
                            &value.original_request.operation_id,
                            now_ms() as u64,
                        )
                    })?
                };
                let transport = TransportEvidence::VerifiedPeer {
                    origin: authorization.wire().invocation.payload.device.clone(),
                };
                let freshness = self
                    .handoff_freshness(authorization.wire(), &transport, stage, verification)
                    .await?;
                Ok(HandoffExecutionCheck::Primary {
                    authorization,
                    freshness,
                })
            }),
            HandoffExecutionPolicy::TaskBusiness {
                source, acceptance, ..
            } => Box::pin(async move {
                let mut context = {
                    let mut store = self.store.lock().await;
                    verification.run(|| {
                        store.begin_task_business_operation_check(
                            &self.identity,
                            source,
                            source.wire(),
                            acceptance,
                            stage,
                            now_ms() as u64,
                        )
                    })?
                };
                let challenge = context.challenge().clone();
                let issued_at_ms = now_ms() as u64;
                let signer =
                    vibeface_secure::Identity::from_secret_hex(&self.identity.secret_key_hex)
                        .map_err(anyhow::Error::msg)?;
                let request = Signed::sign(
                    TaskBusinessReceiptRead {
                        version: AUTH_VERSION,
                        source_header_hash: source.header().hash()?,
                        business_wire_hash: source.wire_hash().clone(),
                        challenge: TaskBusinessChallenge::Submit(challenge.clone()),
                        acceptance: Some(acceptance.clone()),
                        nonce: Nonce::random()?,
                        issued_at_ms,
                        expires_at_ms: issued_at_ms
                            .checked_add(INVOCATION_MAX_AGE_MS)
                            .context("Task operation receipt read deadline overflow")?
                            .min(challenge.expires_at_ms),
                    },
                    &signer,
                )?;
                let mut client = crate::client::TaskBusinessReceiptClient::connect_original(
                    &self.network,
                    &self.identity,
                    source,
                )
                .await?;
                let mut receipts = context.local_receipts().to_vec();
                receipts.extend(client.read(&request).await?);
                let proof = ExecutionHandoffFreshness {
                    challenge,
                    receipts,
                };
                let store = self.store.lock().await;
                let current = verification.run(|| {
                    store.finish_task_business_operation_check(
                        &self.identity,
                        &mut context,
                        &proof,
                        now_ms() as u64,
                    )
                })?;
                Ok(HandoffExecutionCheck::TaskBusiness(current))
            }),
        }
    }

    async fn renew_handoff_execution_policy(
        &self,
        value: &AdmittedExecutionHandoff,
        policy: &SharedHandoffExecutionPolicy,
        verification: &SignatureVerification,
    ) -> Result<u64> {
        let snapshot = policy.lock().await.clone();
        let checked = self
            .check_handoff_execution_policy(value, &snapshot, CheckStage::Renew, verification)
            .await?;
        let _renewal = self.handoff_resolution_lock.lock().await;
        self.apply_handoff_execution_renewal(value, policy, checked, verification)
            .await
    }

    async fn apply_handoff_execution_renewal(
        &self,
        value: &AdmittedExecutionHandoff,
        policy: &SharedHandoffExecutionPolicy,
        checked: HandoffExecutionCheck,
        verification: &SignatureVerification,
    ) -> Result<u64> {
        let deadline = {
            let mut policy = policy.lock().await;
            let mut store = self.store.lock().await;
            match checked {
                HandoffExecutionCheck::Primary {
                    authorization,
                    freshness,
                } => verification.run(|| {
                    store.renew_execution_handoff(
                        &self.identity,
                        &authorization,
                        &freshness,
                        now_ms() as u64,
                    )
                })?,
                HandoffExecutionCheck::TaskBusiness(current) => {
                    let deadline = verification.run(|| {
                        store.renew_execution_handoff_operation_current(
                            &self.identity,
                            &current,
                            now_ms() as u64,
                        )
                    })?;
                    let HandoffExecutionPolicy::TaskBusiness { current: saved, .. } = &mut *policy
                    else {
                        bail!("Task operation renewal changed its execution policy");
                    };
                    *saved = Some(current);
                    deadline
                }
            }
        };
        if value.original_request.action["type"] == "command" {
            self.engine.renew_command(&context(value), deadline).await?;
        } else {
            self.engine
                .renew_operation(&context(value), deadline)
                .await?;
        }
        Ok(deadline)
    }

    pub(super) async fn seal_remote_execution(
        self: &Arc<Self>,
        access: &Access,
        body: &Value,
    ) -> Result<Value> {
        use crate::identity::execution_binding_seal::RemoteExecutionSealPreflight;

        access.require_body(body)?;
        let _seal = self.handoff_resolution_lock.lock().await;
        let preflight = {
            let store = self.store.lock().await;
            access.verification.run(|| {
                store.authorize_remote_execution_seal_request_fence(
                    &self.identity,
                    &access.authorization,
                    &access.freshness,
                    &access.transport,
                    body,
                    now_ms() as u64,
                )
            })?
        };
        let context = match preflight {
            RemoteExecutionSealPreflight::Existing(seal) => {
                return Ok(json!({"seal":seal}));
            }
            RemoteExecutionSealPreflight::Fence(context) => context,
        };
        let (stopped, _) = self.engine.fence_operation(&context).await?;
        self.engine.complete_fenced_operation(&stopped).await?;
        let seal = {
            let mut store = self.store.lock().await;
            access.verification.run(|| {
                store.seal_remote_execution_request_authorized(
                    &self.identity,
                    &access.authorization,
                    &access.freshness,
                    &access.transport,
                    body,
                    &stopped,
                    now_ms() as u64,
                )
            })?
        };
        Ok(json!({"seal":seal}))
    }

    pub(super) async fn complete_execution_dispatch(
        self: &Arc<Self>,
        operation: &OperationRecord,
        verification: &SignatureVerification,
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
        self.finish_with_verification(
            &operation.request,
            operation.state.clone(),
            operation.result.clone(),
            verification,
        )
        .await
    }

    pub(super) async fn collect_handoff_receipts(
        &self,
        route: &SpaceId,
        policies: &[vibeface_secure::authorization::PolicyStamp],
        mut receipts: Vec<Signed<FreshnessReceipt>>,
        kind: &str,
        challenge: Value,
    ) -> Result<Vec<Signed<FreshnessReceipt>>> {
        for receipt in &receipts {
            receipt.payload.validate_currents()?;
        }
        let targets = {
            let store = self.store.lock().await;
            let mut targets = BTreeSet::new();
            for stamp in policies {
                if receipts
                    .iter()
                    .flat_map(|receipt| &receipt.payload.currents)
                    .any(|current| current.domain == stamp.domain)
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
            for receipt in &signed {
                receipt.payload.validate_currents()?;
            }
            receipts.extend(signed);
        }
        receipts.sort_by(|left, right| left.payload.currents.cmp(&right.payload.currents));
        let mut currents: Vec<_> = receipts
            .iter()
            .flat_map(|receipt| &receipt.payload.currents)
            .collect();
        currents.sort();
        ensure!(
            currents == policies.iter().collect::<Vec<_>>(),
            "handoff policy authority is unavailable"
        );
        Ok(receipts)
    }

    async fn handoff_freshness(
        &self,
        wire: &SignedExecutionHandoff,
        transport: &TransportEvidence,
        stage: CheckStage,
        verification: &SignatureVerification,
    ) -> Result<ExecutionHandoffFreshness> {
        let (challenge, receipts) = {
            let mut store = self.store.lock().await;
            verification.run(|| {
                let now = now_ms() as u64;
                let challenge =
                    store.issue_execution_handoff_challenge(wire, transport, stage, now)?;
                let receipts =
                    store.sign_execution_handoff_receipts(&challenge, &self.identity, now)?;
                Ok::<_, anyhow::Error>((challenge, receipts))
            })?
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
        verification: &SignatureVerification,
    ) -> Result<ExecutionHandoffResolutionFreshness> {
        let (challenge, receipts) = {
            let mut store = self.store.lock().await;
            verification.run(|| {
                let now = now_ms() as u64;
                let challenge =
                    store.issue_execution_handoff_resolution_challenge(wire, transport, now)?;
                let receipts = store.sign_execution_handoff_resolution_receipts(
                    &challenge,
                    &self.identity,
                    now,
                )?;
                Ok::<_, anyhow::Error>((challenge, receipts))
            })?
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
        verification: &SignatureVerification,
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
        let (project, _) = original(handoff)?;
        ensure!(
            project.space.as_str() == message.network_id,
            "resolution route differs from its original Project"
        );
        let transport = TransportEvidence::VerifiedPeer { origin };
        let _resolution = self.handoff_resolution_lock.lock().await;
        let completed = {
            let store = self.store.lock().await;
            verification.run(|| {
                if let Some(result) =
                    store.execution_handoff_resolution_status(&wire, &transport, now_ms() as u64)?
                {
                    return Ok(Some(result));
                }
                store.execution_handoff_resolution_authorization(
                    &wire,
                    &transport,
                    now_ms() as u64,
                )?;
                Ok::<_, anyhow::Error>(None)
            })?
        };
        if let Some(result) = completed {
            return Ok(json!(result));
        }
        if wire.resolution.purpose == ExecutionResolutionPurpose::ReconcileTerminal {
            let proof = self
                .handoff_resolution_freshness(&wire, &transport, verification)
                .await?;
            let store = self.store.lock().await;
            return verification.run(|| {
                let auth = store.execution_handoff_resolution_authorization(
                    &wire,
                    &transport,
                    now_ms() as u64,
                )?;
                Ok(json!(
                    store.authorize_execution_handoff_terminal_reconciliation(
                        &auth,
                        &proof,
                        now_ms() as u64,
                    )?
                ))
            });
        }
        let proof = self
            .handoff_resolution_freshness(&wire, &transport, verification)
            .await?;
        let stopped = self
            .fence_execution_handoff_resolution(&wire, &transport, &proof, verification)
            .await?;
        let proof = self
            .handoff_resolution_freshness(&wire, &transport, verification)
            .await?;
        let mut store = self.store.lock().await;
        verification.run(|| {
            Ok(json!(store.resolve_execution_handoff(
                &self.identity,
                &wire,
                &proof,
                &transport,
                &stopped,
                now_ms() as u64,
            )?))
        })
    }

    async fn fence_execution_handoff_resolution(
        self: &Arc<Self>,
        wire: &SignedExecutionHandoffResolution,
        transport: &TransportEvidence,
        proof: &ExecutionHandoffResolutionFreshness,
        verification: &SignatureVerification,
    ) -> Result<crate::engine::StoppedExecution> {
        let (project, operation) = original(&wire.resolution.observation.handoff)?;
        {
            let store = self.store.lock().await;
            verification.run(|| {
                let auth = store.execution_handoff_resolution_authorization(
                    wire,
                    transport,
                    now_ms() as u64,
                )?;
                store.authorize_execution_handoff_resolution_fence(&auth, proof, now_ms() as u64)
            })?;
        }
        let context = ExecutionContext {
            space_id: project.space,
            resource_id: project.resource,
            operation_id: operation,
            shared: true,
            lease_expires_at_ms: proof.challenge.expires_at_ms,
        };
        let command = handoff_is_command(&wire.resolution.observation.handoff)?;
        let mut affected = if command {
            vec![context.operation_id.clone()]
        } else {
            self.engine.affected_operations(&context).await
        };
        let stopped = if command {
            self.engine.fence_command(&context).await
        } else {
            self.engine.fence_operation(&context).await
        };
        if let Ok((_, operations)) = &stopped {
            affected.extend(operations.iter().cloned());
        }
        self.mark_fenced_operations(
            &context,
            affected,
            "execution stopped for an explicit unknown-result resolution",
            stopped.is_ok(),
            verification,
        )
        .await?;
        let (stopped, _) = stopped?;
        self.engine.complete_fenced_operation(&stopped).await?;
        Ok(stopped)
    }

    pub(super) async fn handle_handoff_peer(
        self: &Arc<Self>,
        message: &Inbound,
        origin: DeviceId,
        verification: &SignatureVerification,
    ) -> Result<Value> {
        let envelope = &message.payload;
        if envelope["type"] == "execution_handoff_resolve" {
            return self
                .resolve_handoff_peer(message, origin, verification)
                .await;
        }
        if envelope["type"] == "execution_handoff_resolution_receipts" {
            let challenge: ExecutionHandoffResolutionChallenge =
                serde_json::from_value(envelope["challenge"].clone())?;
            let store = self.store.lock().await;
            return verification.run(|| Ok(json!({"receipts":store
                .sign_execution_handoff_resolution_receipts(&challenge, &self.identity, now_ms() as u64)?})));
        }
        if envelope["type"] == "execution_handoff_receipts" {
            let challenge: ExecutionHandoffChallenge =
                serde_json::from_value(envelope["challenge"].clone())?;
            let store = self.store.lock().await;
            return verification.run(|| Ok(json!({"receipts":store.sign_execution_handoff_receipts(&challenge, &self.identity, now_ms() as u64)?})));
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
        let existing = {
            let store = self.store.lock().await;
            verification
                .run(|| store.find_execution_handoff_by_original(&project, &operation))?
                .is_some()
        };
        let value = if existing || envelope["type"] == "execution_handoff_status" {
            let proof = self
                .handoff_freshness(&wire, &transport, CheckStage::RemoteRead, verification)
                .await?;
            let store = self.store.lock().await;
            verification.run(|| {
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
                )
            })?
        } else {
            ensure!(
                envelope["type"] == "execution_handoff_submit",
                "unsupported execution handoff operation"
            );
            let proof = self
                .handoff_freshness(&wire, &transport, CheckStage::Admit, verification)
                .await?;
            let _admission = self.handoff_resolution_lock.lock().await;
            let mut store = self.store.lock().await;
            verification.run(|| {
                store.accept_execution_handoff(
                    &self.identity,
                    &wire,
                    &proof,
                    &transport,
                    now_ms() as u64,
                )
            })?
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
        Ok(json!(
            self.attach_execution_control(public_result(wire, value)?)
                .await?
        ))
    }

    async fn finish_handoff(
        self: &Arc<Self>,
        value: &AdmittedExecutionHandoff,
        state: OperationState,
        result: Value,
        verification: &SignatureVerification,
    ) -> Result<()> {
        let policy = match self.handoff_execution_policy(value).await {
            Ok(policy) => policy,
            Err(error) => {
                let _completion = self.handoff_resolution_lock.lock().await;
                self.fence_handoff(value).await?;
                return Err(error);
            }
        };
        if value.state == OperationState::Running
            && matches!(
                *policy.lock().await,
                HandoffExecutionPolicy::TaskBusiness { .. }
            )
        {
            let snapshot = policy.lock().await.clone();
            let checked = self
                .check_handoff_execution_policy(value, &snapshot, CheckStage::Renew, verification)
                .await;
            let completion = self.handoff_resolution_lock.lock().await;
            let renewal = match checked {
                Ok(checked) => {
                    self.apply_handoff_execution_renewal(value, &policy, checked, verification)
                        .await
                }
                Err(error) => Err(error),
            };
            if let Err(error) = renewal {
                drop(completion);
                self.stop_handoff_with_policy(
                    value,
                    &format!("execution completion has no current authorization lease: {error:#}"),
                    verification,
                    &policy,
                )
                .await?;
                return Err(error);
            }
            return self
                .finish_handoff_in_locked(value, state, result, verification, &policy)
                .await;
        }
        self.finish_handoff_with_policy(value, state, result, verification, &policy)
            .await
    }

    async fn finish_handoff_with_policy(
        self: &Arc<Self>,
        value: &AdmittedExecutionHandoff,
        state: OperationState,
        result: Value,
        verification: &SignatureVerification,
        policy: &SharedHandoffExecutionPolicy,
    ) -> Result<()> {
        let _completion = self.handoff_resolution_lock.lock().await;
        self.finish_handoff_in_locked(value, state, result, verification, policy)
            .await
    }

    async fn finish_handoff_in_locked(
        self: &Arc<Self>,
        value: &AdmittedExecutionHandoff,
        mut state: OperationState,
        mut result: Value,
        verification: &SignatureVerification,
        policy: &SharedHandoffExecutionPolicy,
    ) -> Result<()> {
        let current = {
            let store = self.store.lock().await;
            verification.run(|| {
                store.execution_handoff_status(
                    &self.identity,
                    value.execution_context.execution_space_id.as_str(),
                    value.execution_context.space_id.as_str(),
                    &value.original_request.operation_id,
                    now_ms() as u64,
                )
            })?
        };
        if current.state.terminal() || current.state == OperationState::Unknown {
            return Ok(());
        }
        let context = context(&current);
        let mut cleanup_affected = Vec::new();
        if state.terminal() && current.state == OperationState::Running {
            let affected = self.handoff_affected(&current).await;
            let completion = if current.original_request.action["type"] == "command" {
                self.engine.complete_command(&context).await
            } else {
                self.engine.complete_operation(&context).await
            };
            if let Err(error) = completion {
                state = OperationState::Unknown;
                result = json!({
                    "error":format!("execution cleanup could not be confirmed: {error:#}"),
                    "observed_result":result,
                });
                cleanup_affected = affected;
            }
        }
        let business = matches!(
            *policy.lock().await,
            HandoffExecutionPolicy::TaskBusiness { .. }
        );
        let mut completion_error = None;
        if !business || state != OperationState::Unknown {
            let persisted = {
                let policy = policy.lock().await;
                self.persist_handoff_result(&current, state, result, verification, &policy)
                    .await
            };
            match persisted {
                Ok(actual) => state = actual,
                Err(error) if business => {
                    state = OperationState::Unknown;
                    completion_error = Some(error);
                }
                Err(error) => return Err(error),
            }
        }
        if state == OperationState::Unknown {
            let mut affected = cleanup_affected;
            affected.extend(self.handoff_affected(&current).await);
            let stopped = self.fence_handoff(&current).await;
            if let Ok((_, operations)) = &stopped {
                affected.extend(operations.iter().cloned());
            }
            if business {
                let reason = completion_error
                    .as_ref()
                    .map(|error| {
                        format!("execution completion has no current authorization: {error:#}")
                    })
                    .unwrap_or_else(|| "execution outcome requires reconciliation".to_owned());
                let policy = policy.lock().await;
                self.persist_handoff_result(
                    &current,
                    OperationState::Unknown,
                    json!({"error":reason,"cancellation_confirmed":stopped.is_ok()}),
                    verification,
                    &policy,
                )
                .await?;
            }
            self.mark_fenced_operations(
                &context,
                affected,
                "execution outcome requires reconciliation",
                stopped.is_ok(),
                verification,
            )
            .await?;
            stopped?;
        }
        if let Some(error) = completion_error {
            return Err(error);
        }
        Ok(())
    }

    async fn persist_handoff_result(
        &self,
        value: &AdmittedExecutionHandoff,
        state: OperationState,
        result: Value,
        verification: &SignatureVerification,
        policy: &HandoffExecutionPolicy,
    ) -> Result<OperationState> {
        let project = ResourceRef {
            space: value.execution_context.space_id.clone(),
            resource: value.execution_context.resource_id.clone(),
        };
        let mut store = self.store.lock().await;
        verification.run(|| {
            let session = value
                .original_request
                .session_id
                .as_deref()
                .map(|session| {
                    store.execution_handoff_session(
                        value.execution_context.execution_space_id.as_str(),
                        &project,
                        &value.original_request.target_node,
                        session,
                    )
                })
                .transpose()?
                .flatten();
            let value = match policy {
                HandoffExecutionPolicy::Primary => store.finish_execution_handoff(
                    &self.identity,
                    value.execution_context.execution_space_id.as_str(),
                    project.space.as_str(),
                    &value.original_request.operation_id,
                    state,
                    result,
                    session.as_ref(),
                    now_ms() as u64,
                )?,
                HandoffExecutionPolicy::TaskBusiness {
                    source, current, ..
                } => {
                    if state == OperationState::Unknown {
                        let reason = result["error"]
                            .as_str()
                            .or_else(|| result["reason"].as_str())
                            .unwrap_or("execution outcome requires reconciliation");
                        store.mark_task_business_handoff_unknown(
                            &self.identity,
                            source,
                            reason,
                            now_ms() as u64,
                        )?
                    } else {
                        store.finish_execution_handoff_operation_current(
                            &self.identity,
                            current
                                .as_ref()
                                .context("Task execution has no current consumed lease")?,
                            state,
                            result,
                            session.as_ref(),
                            now_ms() as u64,
                        )?
                    }
                }
            };
            Ok(value.state)
        })
    }

    async fn execute_handoff(self: Arc<Self>, accepted: AdmittedExecutionHandoff) -> Result<()> {
        let policy = self.handoff_execution_policy(&accepted).await?;
        self.execute_handoff_with_policy(accepted, policy).await
    }

    pub(super) fn execute_handoff_with_business(
        self: Arc<Self>,
        accepted: AdmittedExecutionHandoff,
        session: TaskBusinessPolicySession,
        source: VerifiedTaskBusinessOriginalSource,
        acceptance: RemoteExecutionHistoryAcceptance,
    ) -> futures_util::future::BoxFuture<'static, Result<()>> {
        Box::pin(async move {
            ensure!(
                session.source().header().hash()? == source.header().hash()?
                    && session.source().wire_hash() == source.wire_hash()
                    && source.acceptance() == &acceptance,
                "Task execution changed its completed source or original acceptance"
            );
            let TaskBusinessWire::Submit(wire) = source.wire() else {
                bail!("Task execution requires its original accepted Submit");
            };
            let (project, operation) = original(wire)?;
            ensure!(
                project.space == accepted.execution_context.space_id
                    && project.resource == accepted.execution_context.resource_id
                    && operation == accepted.original_request.operation_id
                    && wire.handoff.execution_resource.space
                        == accepted.execution_context.execution_space_id
                    && wire.handoff.execution_resource.resource
                        == accepted.execution_context.execution_service_resource,
                "Task execution source differs from its accepted execution namespace"
            );
            let policy = Arc::new(Mutex::new(HandoffExecutionPolicy::TaskBusiness {
                source,
                acceptance,
                current: None,
            }));
            self.execute_handoff_with_policy(accepted, policy).await
        })
    }

    fn prepare_handoff_execution<'a>(
        &'a self,
        accepted: &'a AdmittedExecutionHandoff,
        policy: &'a SharedHandoffExecutionPolicy,
        verification: &'a SignatureVerification,
    ) -> futures_util::future::BoxFuture<'a, Result<(AdmittedExecutionHandoff, bool, HandoffProgram)>>
    {
        Box::pin(async move {
            ensure!(
                self.config.roles.contains(&NodeRole::Executor),
                "executor capability is disabled"
            );
            let snapshot = policy.lock().await.clone();
            let checked = self
                .check_handoff_execution_policy(
                    accepted,
                    &snapshot,
                    CheckStage::Start,
                    verification,
                )
                .await?;
            let program = if accepted.original_request.action["type"] == "command" {
                ensure!(
                    accepted.execution_session.is_none()
                        && accepted.original_request.session_id.is_none()
                        && accepted.original_request.task_id.is_none()
                        && accepted.original_request.binding.is_none(),
                    "command must not invent a Task, session or binding"
                );
                HandoffProgram::Command(self.command_spec(&accepted.original_request.action)?)
            } else {
                let session = accepted
                    .execution_session
                    .as_ref()
                    .context("accepted handoff has no local session")?;
                self.allowed_path(&session.cwd)?;
                HandoffProgram::Session {
                    kind: serde_json::from_value(json!(session.executor))?,
                    action: self.engine_action(&accepted.original_request.action)?,
                }
            };
            for resource in &accepted.original_request.resources {
                self.allowed_path(resource)?;
            }
            let (value, fresh, _) = {
                let _start = self.handoff_resolution_lock.lock().await;
                let mut policy = policy.lock().await;
                let mut store = self.store.lock().await;
                match checked {
                    HandoffExecutionCheck::Primary {
                        authorization,
                        freshness,
                    } => verification.run(|| {
                        store.claim_execution_handoff(
                            &self.identity,
                            &authorization,
                            &freshness,
                            now_ms() as u64,
                        )
                    })?,
                    HandoffExecutionCheck::TaskBusiness(current) => {
                        let claimed = verification.run(|| {
                            store.claim_execution_handoff_operation_current(
                                &self.identity,
                                &current,
                                now_ms() as u64,
                            )
                        })?;
                        if claimed.1 {
                            let HandoffExecutionPolicy::TaskBusiness { current: saved, .. } =
                                &mut *policy
                            else {
                                bail!("Task operation claim changed its execution policy");
                            };
                            *saved = Some(current);
                        }
                        claimed
                    }
                }
            };
            Ok::<_, anyhow::Error>((value, fresh, program))
        })
    }

    fn execute_handoff_with_policy(
        self: Arc<Self>,
        accepted: AdmittedExecutionHandoff,
        policy: SharedHandoffExecutionPolicy,
    ) -> futures_util::future::BoxFuture<'static, Result<()>> {
        Box::pin(async move {
            let _work = self.maintenance.enter()?;
            let verification = SignatureVerification::new();
            let preparation = self
                .prepare_handoff_execution(&accepted, &policy, &verification)
                .await;
            let (value, fresh, program) = match preparation {
                Ok(value) => value,
                Err(error) => {
                    let current = {
                        let store = self.store.lock().await;
                        verification.run(|| {
                            store.execution_handoff_status(
                                &self.identity,
                                accepted.execution_context.execution_space_id.as_str(),
                                accepted.execution_context.space_id.as_str(),
                                &accepted.original_request.operation_id,
                                now_ms() as u64,
                            )
                        })?
                    };
                    if current.state == OperationState::Accepted {
                        if matches!(*policy.lock().await, HandoffExecutionPolicy::Primary) {
                            self.finish_handoff_with_policy(
                                &current,
                                OperationState::Rejected,
                                json!({"error":format!("{error:#}")}),
                                &verification,
                                &policy,
                            )
                            .await?;
                        } else {
                            self.stop_handoff_with_policy(
                                &current,
                                &format!("accepted Task execution could not start: {error:#}"),
                                &verification,
                                &policy,
                            )
                            .await?;
                        }
                    }
                    return Err(error);
                }
            };
            if !fresh {
                return Ok(());
            }
            let (kind, action) = match program {
                HandoffProgram::Session { kind, action } => (kind, action),
                HandoffProgram::Command(spec) => {
                    self.supervise_handoff(value.clone(), policy.clone()).await;
                    let result = self
                        .engine
                        .execute_command_authorized(&context(&value), &spec)
                        .await;
                    let (state, result) = match result {
                        Ok(output) => (
                            if output.exit_code == Some(0) {
                                OperationState::Succeeded
                            } else {
                                OperationState::Failed
                            },
                            serde_json::to_value(output)?,
                        ),
                        Err(error) => (
                            if error.uncertain {
                                OperationState::Unknown
                            } else {
                                OperationState::Failed
                            },
                            json!({"error":error.message}),
                        ),
                    };
                    return self
                        .finish_handoff_with_policy(
                            &value,
                            state,
                            result,
                            &SignatureVerification::new(),
                            &policy,
                        )
                        .await;
                }
            };
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
            self.supervise_handoff(value.clone(), policy.clone()).await;
            let store = self.store.clone();
            let execution = value.execution_context.execution_space_id.clone();
            let project = ResourceRef {
                space: context.space_id.clone(),
                resource: context.resource_id.clone(),
            };
            let operation = context.operation_id.clone();
            let native_verification = verification.clone();
            let native_policy = policy.clone();
            let result = self
                .engine
                .execute_authorized(&context, &native, action.clone(), move |id| async move {
                    let policy = native_policy.lock().await;
                    let mut store = store.lock().await;
                    native_verification.run(|| match &*policy {
                        HandoffExecutionPolicy::Primary => store.persist_execution_handoff_native(
                            execution.as_str(),
                            &project,
                            &operation,
                            &id,
                            now_ms() as u64,
                        ),
                        HandoffExecutionPolicy::TaskBusiness { current, .. } => store
                            .persist_execution_handoff_native_operation_current(
                                current
                                    .as_ref()
                                    .context("Task execution has no current consumed lease")?,
                                &id,
                                now_ms() as u64,
                            ),
                    })?;
                    Ok(())
                })
                .await;
            let completion_verification = SignatureVerification::new();
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
                    ) && let Ok(Some(observed)) = self
                        .engine
                        .read_turn_authorized(&context, native, turn)
                        .await
                        && matches!(
                            observed["status"].as_str(),
                            Some("completed" | "failed" | "interrupted")
                        )
                    {
                        let state = if observed["status"] == "completed" {
                            OperationState::Succeeded
                        } else {
                            OperationState::Failed
                        };
                        self.finish_handoff_with_policy(
                            &value,
                            state,
                            provider_result(observed, Some(native)),
                            &completion_verification,
                            &policy,
                        )
                        .await?;
                    }
                }
                Ok(result) => {
                    self.finish_handoff_with_policy(
                        &value,
                        OperationState::Succeeded,
                        provider_result(result.result, result.native_session_id.as_deref()),
                        &completion_verification,
                        &policy,
                    )
                    .await?
                }
                Err(error) => {
                    self.finish_handoff_with_policy(
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
                        &completion_verification,
                        &policy,
                    )
                    .await?
                }
            }
            Ok(())
        })
    }

    async fn supervise_handoff(
        self: &Arc<Self>,
        value: AdmittedExecutionHandoff,
        policy: SharedHandoffExecutionPolicy,
    ) {
        let context = context(&value);
        let key = (context.space_id.to_string(), context.operation_id.clone());
        if !self.execution_leases.lock().await.insert(key.clone()) {
            return;
        }
        let node = self.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(5)).await;
                let verification = SignatureVerification::new();
                let status = {
                    let store = node.store.lock().await;
                    verification.run(|| {
                        store.execution_handoff_status(
                            &node.identity,
                            value.execution_context.execution_space_id.as_str(),
                            context.space_id.as_str(),
                            &context.operation_id,
                            now_ms() as u64,
                        )
                    })
                };
                match status {
                    Ok(current) if current.state.terminal() => break,
                    Ok(current) if current.state == OperationState::Running => {}
                    _ => {
                        let _ = node
                            .stop_handoff_with_policy(
                                &value,
                                "execution outcome requires reconciliation",
                                &verification,
                                &policy,
                            )
                            .await;
                        break;
                    }
                }
                let renewal = node
                    .renew_handoff_execution_policy(&value, &policy, &verification)
                    .await;
                if let Err(error) = renewal {
                    let _ = node
                        .stop_handoff_with_policy(
                            &value,
                            &format!("authorization lease could not renew: {error:#}"),
                            &verification,
                            &policy,
                        )
                        .await;
                    break;
                }
            }
            node.execution_leases.lock().await.remove(&key);
        });
    }

    async fn stop_handoff(
        self: &Arc<Self>,
        value: &AdmittedExecutionHandoff,
        reason: &str,
        verification: &SignatureVerification,
    ) -> Result<()> {
        let policy = match self.handoff_execution_policy(value).await {
            Ok(policy) => policy,
            Err(error) => {
                let _completion = self.handoff_resolution_lock.lock().await;
                self.fence_handoff(value).await?;
                return Err(error);
            }
        };
        self.stop_handoff_with_policy(value, reason, verification, &policy)
            .await
    }

    async fn stop_handoff_with_policy(
        self: &Arc<Self>,
        value: &AdmittedExecutionHandoff,
        reason: &str,
        verification: &SignatureVerification,
        policy: &SharedHandoffExecutionPolicy,
    ) -> Result<()> {
        let _completion = self.handoff_resolution_lock.lock().await;
        let context = context(value);
        let mut affected = self.handoff_affected(value).await;
        let stopped = self.fence_handoff(value).await;
        if let Ok((_, operations)) = &stopped {
            affected.extend(operations.iter().cloned());
        }
        let current = {
            let store = self.store.lock().await;
            verification.run(|| {
                store.execution_handoff_status(
                    &self.identity,
                    value.execution_context.execution_space_id.as_str(),
                    value.execution_context.space_id.as_str(),
                    &value.original_request.operation_id,
                    now_ms() as u64,
                )
            })?
        };
        if current.state == OperationState::Running
            || (current.state == OperationState::Accepted
                && matches!(
                    *policy.lock().await,
                    HandoffExecutionPolicy::TaskBusiness { .. }
                ))
        {
            let policy = policy.lock().await;
            self.persist_handoff_result(
                &current,
                OperationState::Unknown,
                json!({"error":reason,"cancellation_confirmed":stopped.is_ok()}),
                verification,
                &policy,
            )
            .await?;
        }
        affected.retain(|operation| operation != &context.operation_id);
        self.mark_fenced_operations(&context, affected, reason, stopped.is_ok(), verification)
            .await?;
        stopped.map(|_| ())
    }

    pub(super) async fn mark_fenced_operations(
        self: &Arc<Self>,
        context: &ExecutionContext,
        mut affected: Vec<String>,
        reason: &str,
        confirmed: bool,
        verification: &SignatureVerification,
    ) -> Result<()> {
        let project = ResourceRef {
            space: context.space_id.clone(),
            resource: context.resource_id.clone(),
        };
        affected.sort();
        affected.dedup();
        for operation in affected {
            let handoff = {
                let store = self.store.lock().await;
                verification.run(|| {
                    store
                        .find_execution_handoff_by_original(&project, &operation)?
                        .map(|scope| {
                            store.execution_handoff_status(
                                &self.identity,
                                scope.execution_space_id.as_str(),
                                project.space.as_str(),
                                &operation,
                                now_ms() as u64,
                            )
                        })
                        .transpose()
                })?
            };
            if let Some(current) = handoff {
                if current.state == OperationState::Running {
                    let policy = self.handoff_execution_policy(&current).await?;
                    let policy = policy.lock().await;
                    self.persist_handoff_result(
                        &current,
                        OperationState::Unknown,
                        json!({"error":reason,"cancellation_confirmed":confirmed}),
                        verification,
                        &policy,
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
                self.finish_with_verification(
                    &direct.request,
                    OperationState::Unknown,
                    json!({"error":reason,"cancellation_confirmed":confirmed}),
                    verification,
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
        let verification = SignatureVerification::new();
        let scope = {
            let store = self.store.lock().await;
            verification.run(|| store.find_execution_handoff_by_original(&project, operation))?
        };
        let Some(scope) = scope else {
            return Ok(());
        };
        let value = {
            let store = self.store.lock().await;
            verification.run(|| {
                store.execution_handoff_status(
                    &self.identity,
                    scope.execution_space_id.as_str(),
                    project.space.as_str(),
                    operation,
                    now_ms() as u64,
                )
            })?
        };
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
                .stop_handoff(
                    &value,
                    "executor disconnected; reconcile before retrying",
                    &verification,
                )
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
                &verification,
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
            let verification = SignatureVerification::new();
            if value.state == OperationState::Unknown {
                if let Err(error) = self
                    .stop_handoff(&value, "unresolved execution after restart", &verification)
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
    use crate::identity::execution_handoff::tests::Fixture;
    use vibeface_secure::{
        Identity,
        authorization::{EXECUTION_LEASE_MS, MAX_PROOF_SIGNATURES},
        signature_verification::{SignatureVerification, SignatureVerificationCounts},
    };

    #[derive(PartialEq)]
    struct HandoffStateSnapshot {
        total_changes: u64,
        rows: Vec<Vec<Vec<rusqlite::types::Value>>>,
    }

    impl HandoffStateSnapshot {
        fn read(store: &Store) -> Result<Self> {
            let mut rows = Vec::new();
            for table in [
                "execution_handoffs",
                "execution_handoff_nonces",
                "execution_handoff_reservations",
                "execution_handoff_challenges",
                "execution_handoff_resolution_challenges",
                "execution_handoff_resolutions",
                "execution_handoff_sessions",
            ] {
                let mut statement = store
                    .connection
                    .prepare(&format!("SELECT * FROM {table} ORDER BY rowid"))?;
                let columns = statement.column_count();
                rows.push(
                    statement
                        .query_map([], |row| {
                            (0..columns)
                                .map(|column| row.get(column))
                                .collect::<rusqlite::Result<Vec<_>>>()
                        })?
                        .collect::<rusqlite::Result<Vec<_>>>()?,
                );
            }
            Ok(Self {
                total_changes: store.connection.total_changes(),
                rows,
            })
        }
    }

    struct PreparedHandoffRuntime {
        fixture: Fixture,
        node: Arc<Runtime>,
        wire: SignedExecutionHandoffResolution,
        proof: ExecutionHandoffResolutionFreshness,
        context: ExecutionContext,
        docker: PathBuf,
        containers: Vec<String>,
    }

    impl PreparedHandoffRuntime {
        async fn open() -> Result<Self> {
            let mut fixture = Fixture::with_executor("claude_code")?;
            fixture.start_unknown()?;
            let wire = fixture.resolution(ExecutionResolutionPurpose::ResolveUnknown)?;
            let active_at = wire
                .invocation
                .payload
                .not_before_ms
                .max(wire.resolution.account_admission.admitted_at_ms);
            tokio::time::sleep(Duration::from_millis(
                active_at.saturating_sub(now_ms() as u64),
            ))
            .await;
            let proof = fixture.resolution_freshness(&wire, now_ms() as u64)?;
            let dir = fixture.executor.directory.join("prefence-runtime");
            config::private_dir(&dir)?;
            fixture.store.connection.execute(
                "VACUUM main INTO ?",
                [dir.join("node.sqlite3")
                    .to_str()
                    .context("test database path must be UTF-8")?],
            )?;
            config::private_write(
                &dir.join("data-epoch.json"),
                &serde_json::to_vec(&config::data_format())?,
            )?;
            config::private_write(
                &dir.join("identity.json"),
                &serde_json::to_vec(&fixture.executor.node)?,
            )?;
            let docker = PathBuf::from(std::env::var("VIBEFACE_ISOLATION_TEST_DOCKER")?);
            let mut cfg = Config::local(
                "127.0.0.1:4790".parse()?,
                vec![fixture.executor.project.clone()],
                false,
            );
            cfg.roles = vec![NodeRole::Executor];
            cfg.codex_socket = Some(dir.join("unavailable.sock"));
            cfg.codex_bin = dir.join("missing-codex");
            cfg.claude_bin = dir.join("missing-claude");
            cfg.cursor_bin = dir.join("missing-cursor");
            cfg.execution_isolation = Some(crate::engine::ExecutionIsolationConfig {
                image: std::env::var("VIBEFACE_ISOLATION_TEST_IMAGE")?,
                docker_bin: docker.clone(),
                egress: None,
            });
            cfg.save(&dir)?;
            let node = Arc::new(Runtime::open(&dir).await?);
            let (project, operation) = original(&fixture.wire)?;
            let context = ExecutionContext {
                space_id: project.space,
                resource_id: project.resource,
                operation_id: operation,
                shared: true,
                lease_expires_at_ms: now_ms() as u64 + EXECUTION_LEASE_MS,
            };
            Ok(Self {
                fixture,
                node,
                wire,
                proof,
                context,
                docker,
                containers: Vec::new(),
            })
        }

        async fn prepare(&mut self, context: &ExecutionContext) -> Result<String> {
            let id = self
                .node
                .engine
                .prepare_operation_for_test(context, &self.fixture.executor.project)
                .await?;
            self.containers.push(id.clone());
            self.assert_running(&id).await?;
            Ok(id)
        }

        async fn inspect(&self, id: &str) -> Result<Option<Value>> {
            let output = tokio::time::timeout(
                Duration::from_secs(15),
                tokio::process::Command::new(&self.docker)
                    .args(["container", "inspect", id])
                    .kill_on_drop(true)
                    .output(),
            )
            .await??;
            if !output.status.success() {
                let error = String::from_utf8_lossy(&output.stderr).to_ascii_lowercase();
                ensure!(
                    error.contains("no such container") || error.contains("no such object"),
                    "test could not inspect its container"
                );
                return Ok(None);
            }
            let mut containers: Vec<Value> = serde_json::from_slice(&output.stdout)?;
            ensure!(
                containers.len() == 1 && containers[0]["Id"] == id,
                "test inspected another container"
            );
            Ok(containers.pop())
        }

        async fn assert_running(&self, id: &str) -> Result<()> {
            let container = self
                .inspect(id)
                .await?
                .context("prepared test container disappeared")?;
            ensure!(
                container["State"]["Running"] == true
                    && container["State"]["Pid"]
                        .as_u64()
                        .is_some_and(|pid| pid > 0),
                "prepared test container is no longer running"
            );
            Ok(())
        }

        async fn snapshot(&self) -> Result<HandoffStateSnapshot> {
            HandoffStateSnapshot::read(&*self.node.store.lock().await)
        }

        async fn assert_unfenced(&self, id: &str, before: &HandoffStateSnapshot) -> Result<()> {
            self.assert_running(id).await?;
            ensure!(
                !self
                    .node
                    .engine
                    .operation_is_fenced_for_test(&self.context)
                    .await,
                "a rejected request fenced the original generation"
            );
            ensure!(
                *before == self.snapshot().await?,
                "a rejected request changed handoff state, result, nonce, reservations, or challenge consumption"
            );
            Ok(())
        }

        fn bounded_counts(
            verification: &SignatureVerification,
        ) -> Result<SignatureVerificationCounts> {
            let counts = verification.counts();
            ensure!(
                counts.actual_verifications <= MAX_PROOF_SIGNATURES,
                "runtime request exceeded the signature verification limit"
            );
            Ok(counts)
        }

        async fn reject(
            &self,
            proof: &ExecutionHandoffResolutionFreshness,
            expected: &str,
        ) -> Result<SignatureVerificationCounts> {
            let verification = SignatureVerification::new();
            let error = match self
                .node
                .fence_execution_handoff_resolution(
                    &self.wire,
                    &self.fixture.transport,
                    proof,
                    &verification,
                )
                .await
            {
                Ok(_) => bail!("invalid authorization stopped a prepared container"),
                Err(error) => error,
            };
            ensure!(
                error
                    .chain()
                    .any(|cause| cause.to_string().contains(expected)),
                "runtime rejected before expected gate {expected}: {error:#}; actual_verifications={}",
                verification.counts().actual_verifications
            );
            Self::bounded_counts(&verification)
        }

        async fn cleanup(&self) -> Result<()> {
            let mut failed = false;
            for id in &self.containers {
                let removed = async {
                    let output = tokio::time::timeout(
                        Duration::from_secs(15),
                        tokio::process::Command::new(&self.docker)
                            .args(["rm", "--force", id])
                            .kill_on_drop(true)
                            .output(),
                    )
                    .await??;
                    let error = String::from_utf8_lossy(&output.stderr).to_ascii_lowercase();
                    ensure!(
                        output.status.success() || error.contains("no such container"),
                        "test container cleanup failed"
                    );
                    ensure!(
                        self.inspect(id).await?.is_none(),
                        "test container survived cleanup"
                    );
                    Ok::<_, anyhow::Error>(())
                }
                .await;
                failed |= removed.is_err();
            }
            ensure!(
                !failed,
                "one or more exact test containers could not be removed"
            );
            Ok(())
        }
    }

    #[tokio::test]
    #[ignore = "requires the pinned isolation fixture image and a local Docker daemon"]
    async fn production_resolution_prefence_rejects_missing_expired_and_revoked_authorization()
    -> Result<()> {
        let mut prepared = PreparedHandoffRuntime::open().await?;
        let outcome = async {
            let original_context = prepared.context.clone();
            let id = prepared.prepare(&original_context).await?;
            {
                let store = prepared.node.store.lock().await;
                let state: String = store.connection.query_row("SELECT state FROM execution_handoffs", [], |row| row.get(0))?;
                ensure!(state == "unknown", "runtime fixture did not preserve the original unknown result");
            }
            let before = prepared.snapshot().await?;
            let mut missing = prepared.proof.clone();
            missing.receipts.pop().context("fixture has no freshness receipts")?;
            let missing_counts = prepared.reject(&missing, "resolution requires both current authority closures").await?;
            prepared.assert_unfenced(&id, &before).await?;

            let mut expired = prepared.proof.clone();
            let receipt = expired.receipts.first_mut().context("fixture has no authority receipt")?;
            let local = if receipt.issuer.as_str() == prepared.fixture.project.node.public_key_hex()? {
                &prepared.fixture.project.node
            } else {
                ensure!(receipt.issuer.as_str() == prepared.fixture.executor.node.public_key_hex()?, "fixture receipt has an unknown authority");
                &prepared.fixture.executor.node
            };
            let signer = Identity::from_secret_hex(&local.secret_key_hex).map_err(anyhow::Error::msg)?;
            let mut shortened = receipt.payload.clone();
            shortened.expires_at_ms = expired.challenge.expires_at_ms.min(now_ms() as u64 + 1_000);
            let expires_at = shortened.expires_at_ms;
            *receipt = Signed::sign(shortened, &signer)?;
            let verification = SignatureVerification::new();
            {
                let store = prepared.node.store.lock().await;
                verification.run(|| {
                    let at = now_ms() as u64;
                    let auth = store.execution_handoff_resolution_authorization(&prepared.wire, &prepared.fixture.transport, at)?;
                    store.authorize_execution_handoff_resolution_fence(&auth, &expired, at)
                })?;
            }
            PreparedHandoffRuntime::bounded_counts(&verification)?;
            ensure!(before == prepared.snapshot().await?, "valid freshness inspection consumed runtime state");
            tokio::time::sleep(Duration::from_millis(expires_at.saturating_sub(now_ms() as u64) + 1)).await;
            let expired_counts = prepared.reject(&expired, "Expired").await?;
            prepared.assert_unfenced(&id, &before).await?;

            let verification = SignatureVerification::new();
            let stopped = prepared.node.fence_execution_handoff_resolution(&prepared.wire, &prepared.fixture.transport, &prepared.proof, &verification).await?;
            ensure!(prepared.inspect(&id).await?.is_none(), "authorized resolution did not clean the original container");
            ensure!(prepared.node.engine.operation_is_fenced_for_test(&prepared.context).await, "authorized resolution did not permanently fence the old operation");
            ensure!(before == prepared.snapshot().await?, "stopping an already unknown operation consumed its resolution or released its reservations");
            let result = {
                let mut store = prepared.node.store.lock().await;
                verification.run(|| store.resolve_execution_handoff(&prepared.node.identity, &prepared.wire, &prepared.proof, &prepared.fixture.transport, &stopped, now_ms() as u64))?
            };
            let legal_counts = PreparedHandoffRuntime::bounded_counts(&verification)?;
            {
                let store = prepared.node.store.lock().await;
                let state: String = store.connection.query_row("SELECT state FROM execution_handoffs", [], |row| row.get(0))?;
                let consumed: bool = store.connection.query_row("SELECT consumed FROM execution_handoff_resolution_challenges WHERE nonce=?", [prepared.proof.challenge.nonce.as_str()], |row| row.get(0))?;
                let reservations: u64 = store.connection.query_row("SELECT COUNT(*) FROM execution_handoff_reservations", [], |row| row.get(0))?;
                let audit: u64 = store.connection.query_row("SELECT COUNT(*) FROM execution_handoff_resolutions", [], |row| row.get(0))?;
                ensure!(state == "succeeded" && consumed && reservations == 0 && audit == 1, "resolution did not atomically record its outcome, consume its challenge, and release reservations");
            }
            let mut replacement = prepared.context.clone();
            replacement.operation_id = "replacement-after-explicit-resolution".into();
            replacement.lease_expires_at_ms = now_ms() as u64 + EXECUTION_LEASE_MS;
            let replacement_id = prepared.prepare(&replacement).await?;
            ensure!(replacement_id != id, "replacement reused the stopped container generation");
            let before_retry = prepared.snapshot().await?;
            let message = Inbound {
                network_id: prepared.context.space_id.to_string(), origin: prepared.fixture.project.node.node_id()?,
                request_id: "retry-successful-resolution".into(),
                payload: json!({"type":"execution_handoff_resolve","resolution":prepared.wire}), broadcast: false,
                verification: SignatureVerification::new(),
            };
            let retry_verification = SignatureVerification::new();
            let response = prepared.node.handle_handoff_peer(&message, prepared.wire.invocation.payload.device.clone(), &retry_verification).await?;
            let retried: ExecutionHandoffResolutionResult = serde_json::from_value(response)?;
            ensure!(retried == result, "resolution retry did not return its original committed receipt");
            prepared.assert_running(&replacement_id).await?;
            ensure!(!prepared.node.engine.operation_is_fenced_for_test(&replacement).await, "successful resolution retry fenced the replacement generation");
            ensure!(before_retry == prepared.snapshot().await?, "successful resolution retry changed the database");
            let retry_counts = PreparedHandoffRuntime::bounded_counts(&retry_verification)?;
            println!("PREFENCE_RUNTIME_PROOF {}", json!({"missing_receipt":missing_counts,"expired_receipt":expired_counts,"authorized_resolution":legal_counts,"successful_retry":retry_counts,"prepared_containers":2,"rejected_requests_preserved_database":2,"successful_resolution_audit_rows":1,"retry_preserved_replacement":true,"provider_launches":0}));
            Ok::<_, anyhow::Error>(())
        }.await;
        let cleaned = prepared.cleanup().await;
        outcome?;
        cleaned?;

        let mut revoked = PreparedHandoffRuntime::open().await?;
        let outcome = async {
            let original_context = revoked.context.clone();
            let id = revoked.prepare(&original_context).await?;
            {
                let mut store = revoked.node.store.lock().await;
                revoked.fixture.executor.revoke_client(&mut store)?;
            }
            let before = revoked.snapshot().await?;
            let counts = revoked.reject(&revoked.proof, "Revoked").await?;
            revoked.assert_unfenced(&id, &before).await?;
            println!("PREFENCE_RUNTIME_PROOF {}", json!({"revoked_resolver":counts,"prepared_containers":1,"rejected_requests_preserved_database":1,"provider_launches":0}));
            Ok::<_, anyhow::Error>(())
        }.await;
        let cleaned = revoked.cleanup().await;
        outcome?;
        cleaned?;
        Ok(())
    }

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
