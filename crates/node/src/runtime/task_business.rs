use super::*;
use crate::identity::task_business_policy::{
    TaskBusinessPolicySession, TaskBusinessPolicySourceContext, TaskBusinessRequest,
    TaskBusinessResponse,
};
use vibeface_secure::authorization::{
    execution_handoff::{
        ExecutionHandoffFreshness, ExecutionHandoffResolutionFreshness, ExecutionHandoffResult,
        ExecutionResolutionPurpose,
    },
    execution_observation::ExecutionObservationFreshness,
    node_execution_policy::ActualPolicyNoisePeer,
    task_business_policy::{
        TaskBusinessWire,
        receipt::{TaskBusinessChallenge, TaskBusinessReceiptRead},
    },
};

fn transport(
    identity: &LocalIdentity,
    session: &TaskBusinessPolicySession,
    peer: &ActualPolicyNoisePeer,
    wire: &TaskBusinessWire,
) -> Result<TransportEvidence> {
    let scope = wire.scope()?;
    let source = peer.public_key();
    let device = DeviceId::from_key(source);
    ensure!(
        source == &scope.source_node_public_key
            && source == &session.source().header().issuer
            && wire.invocation().payload.device == device
            && wire.invocation().payload.presenter == device
            && scope.target_public_key.as_str() == identity.public_key_hex()?,
        "Task business presenter differs from its completed Noise connection"
    );
    Ok(TransportEvidence::VerifiedPeer { origin: device })
}

fn result(
    wire: vibeface_secure::authorization::execution_handoff::SignedExecutionHandoff,
    value: crate::identity::execution_handoff::AdmittedExecutionHandoff,
) -> Result<ExecutionHandoffResult> {
    Ok(ExecutionHandoffResult {
        handoff: wire,
        receipt: value
            .receipt
            .context("executor did not sign its current state")?,
        result: value.result,
    })
}

impl Runtime {
    pub(super) async fn open_task_business_client(
        &self,
        source: &mut TaskBusinessPolicySourceContext,
    ) -> Result<crate::client::TaskBusinessClient> {
        let mut client = crate::client::TaskBusinessClient::connect(
            &self.network,
            &self.identity,
            source.export(),
        )
        .await?;
        client
            .exchange(&TaskBusinessRequest::SourceBegin {
                header: source.header().clone(),
            })
            .await?;
        loop {
            let chunk = {
                let mut store = self.store.lock().await;
                SignatureVerification::new().run(|| {
                    store.next_task_business_source(&self.identity, source, now_ms() as u64)
                })?
            };
            if let Some(page) = chunk.page {
                client
                    .exchange(&TaskBusinessRequest::SourcePage { page })
                    .await?;
            }
            if let Some(witness) = chunk.witness {
                client
                    .exchange(&TaskBusinessRequest::SourceFinish { witness })
                    .await?;
                break;
            }
        }
        Ok(client)
    }

    pub(super) async fn load_task_business_execution_original(
        &self,
        execution_space: &str,
        original_space: &str,
        operation_id: &str,
    ) -> Result<Option<(
        vibeface_secure::authorization::task_business_policy::original::VerifiedTaskBusinessOriginalSource,
        vibeface_secure::authorization::remote_execution_history::RemoteExecutionHistoryAcceptance,
    )>>{
        let (acceptance, mut recovery) = {
            let store = self.store.lock().await;
            let (wire, acceptance) = store.execution_handoff_original_acceptance(
                execution_space,
                original_space,
                operation_id,
            )?;
            let grant = store
                .authorization_state()
                .grant(&wire.invocation.payload.grant_leaf)
                .context("NeedPolicySync: accepted execution grant is missing")?;
            if grant.payload.execution_source.is_none() {
                return Ok(None);
            }
            let recovery = store.begin_task_business_original_recovery(
                &self.identity,
                &TaskBusinessWire::Submit(wire),
                &acceptance,
                now_ms() as u64,
            )?;
            (acceptance, recovery)
        };
        loop {
            let source = {
                let store = self.store.lock().await;
                store.advance_task_business_original_recovery(&mut recovery)?
            };
            if let Some(source) = source {
                return Ok(Some((source, acceptance)));
            }
            tokio::task::yield_now().await;
        }
    }

    pub(crate) async fn task_business(
        self: &Arc<Self>,
        session: &TaskBusinessPolicySession,
        peer: &ActualPolicyNoisePeer,
        wire: TaskBusinessWire,
    ) -> Result<TaskBusinessResponse> {
        self.begin_task_business(session, peer, wire, false).await
    }

    pub(crate) async fn task_business_status(
        self: &Arc<Self>,
        session: &TaskBusinessPolicySession,
        peer: &ActualPolicyNoisePeer,
        wire: TaskBusinessWire,
    ) -> Result<TaskBusinessResponse> {
        self.begin_task_business(session, peer, wire, true).await
    }

    async fn begin_task_business(
        self: &Arc<Self>,
        session: &TaskBusinessPolicySession,
        peer: &ActualPolicyNoisePeer,
        wire: TaskBusinessWire,
        status: bool,
    ) -> Result<TaskBusinessResponse> {
        let _work = self.maintenance.enter()?;
        let evidence = transport(&self.identity, session, peer, &wire)?;
        let verification = SignatureVerification::new();
        let mut store = self.store.lock().await;
        verification.run(|| {
            let now = now_ms() as u64;
            let view = store.task_business_policy_view(&self.identity, session, &wire, now)?;
            let challenge = match &wire {
                TaskBusinessWire::Submit(submit) => {
                    let source = submit.handoff.source_statement()?;
                    let existing = store
                        .find_execution_handoff_by_original(&source.project, &source.operation_id)?
                        .is_some();
                    if status && !existing {
                        return Ok(TaskBusinessResponse::Status(json!({"state":"not_found"})));
                    }
                    let stage = if status || existing {
                        CheckStage::RemoteRead
                    } else {
                        ensure!(
                            self.config.roles.contains(&NodeRole::Executor),
                            "executor capability is disabled"
                        );
                        CheckStage::Admit
                    };
                    TaskBusinessChallenge::Submit(store.issue_execution_handoff_challenge_scoped(
                        &view, submit, &evidence, stage, now,
                    )?)
                }
                TaskBusinessWire::Observe(observe) => TaskBusinessChallenge::Observe(
                    store.issue_execution_observation_challenge_scoped(
                        &view,
                        &self.identity,
                        observe,
                        &evidence,
                        now,
                    )?,
                ),
                TaskBusinessWire::Resolve(resolve) => {
                    let completed = store.execution_handoff_resolution_status_scoped(
                        &view, resolve, &evidence, now,
                    )?;
                    if let Some(completed) = completed {
                        return Ok(TaskBusinessResponse::Status(serde_json::to_value(
                            completed,
                        )?));
                    }
                    if status {
                        return Ok(TaskBusinessResponse::Status(json!({"state":"not_found"})));
                    }
                    TaskBusinessChallenge::Resolve(
                        store.issue_execution_handoff_resolution_challenge_scoped(
                            &view, resolve, &evidence, now,
                        )?,
                    )
                }
            };
            Ok(TaskBusinessResponse::Challenge {
                source_context: session.source().header().hash()?,
                business_wire_hash: wire.hash()?,
                challenge,
            })
        })
    }

    pub(crate) async fn task_business_receipts(
        self: &Arc<Self>,
        session: &TaskBusinessPolicySession,
        peer: &ActualPolicyNoisePeer,
        wire: TaskBusinessWire,
        challenge: TaskBusinessChallenge,
        mut receipts: Vec<Signed<FreshnessReceipt>>,
    ) -> Result<TaskBusinessResponse> {
        let _work = self.maintenance.enter()?;
        let evidence = transport(&self.identity, session, peer, &wire)?;
        let verification = SignatureVerification::new();
        let _resolution = self.handoff_resolution_lock.lock().await;
        let (view, local) = {
            let store = self.store.lock().await;
            verification.run(|| {
                let now = now_ms() as u64;
                let view = store.task_business_policy_view(&self.identity, session, &wire, now)?;
                let local = match (&wire, &challenge) {
                    (TaskBusinessWire::Submit(wire), TaskBusinessChallenge::Submit(challenge)) => {
                        store.sign_execution_handoff_receipts_scoped(
                            &view,
                            wire,
                            challenge,
                            &self.identity,
                            now,
                        )?
                    }
                    (
                        TaskBusinessWire::Observe(wire),
                        TaskBusinessChallenge::Observe(challenge),
                    ) => store.sign_execution_observation_receipts_scoped(
                        &view,
                        wire,
                        &evidence,
                        challenge,
                        &self.identity,
                        now,
                    )?,
                    (
                        TaskBusinessWire::Resolve(wire),
                        TaskBusinessChallenge::Resolve(challenge),
                    ) => store.sign_execution_handoff_resolution_receipts_scoped(
                        &view,
                        wire,
                        &evidence,
                        challenge,
                        &self.identity,
                        now,
                    )?,
                    _ => bail!("Task business receipts changed their typed challenge"),
                };
                Ok::<_, anyhow::Error>((view, local))
            })?
        };
        receipts.extend(local);
        for receipt in &receipts {
            receipt.payload.validate_currents()?;
        }
        receipts.sort_by(|left, right| left.payload.currents.cmp(&right.payload.currents));
        match (wire, challenge) {
            (TaskBusinessWire::Submit(wire), TaskBusinessChallenge::Submit(challenge)) => {
                let proof = ExecutionHandoffFreshness {
                    challenge,
                    receipts,
                };
                let mut store = self.store.lock().await;
                let now = now_ms() as u64;
                if proof.challenge.stage == CheckStage::RemoteRead {
                    let source = wire.handoff.source_statement()?;
                    let auth = verification.run(|| {
                        store.execution_handoff_authorization_scoped(
                            &view,
                            wire.handoff.execution_resource.space.as_str(),
                            source.project.space.as_str(),
                            &source.operation_id,
                            now,
                        )
                    })?;
                    let value = verification.run(|| {
                        store.execution_handoff_status_scoped(
                            &view,
                            &self.identity,
                            &auth,
                            &proof,
                            now,
                        )
                    })?;
                    let result = result(wire, value)?;
                    drop(store);
                    return Ok(TaskBusinessResponse::Status(serde_json::to_value(
                        self.attach_execution_control(result).await?,
                    )?));
                }
                ensure!(
                    proof.challenge.stage == CheckStage::Admit,
                    "unexpected Task business stage"
                );
                let value = verification.run(|| {
                    store.accept_execution_handoff_scoped(
                        &view,
                        &self.identity,
                        &wire,
                        &proof,
                        &evidence,
                        now,
                    )
                })?;
                let response = serde_json::to_value(result(wire.clone(), value.clone())?)?;
                drop(store);
                if value.state == OperationState::Accepted {
                    self.spawn_task_business_handoff(session, wire, value)
                        .await?;
                }
                Ok(TaskBusinessResponse::Business(response))
            }
            (TaskBusinessWire::Observe(wire), TaskBusinessChallenge::Observe(challenge)) => {
                let proof = ExecutionObservationFreshness {
                    challenge,
                    receipts,
                };
                let store = self.store.lock().await;
                let actual = verification.run(|| {
                    store.observe_execution_handoff_scoped(
                        &view,
                        &self.identity,
                        &wire,
                        &proof,
                        &evidence,
                        now_ms() as u64,
                    )
                })?;
                drop(store);
                let actual = match actual {
                    Some(actual) => Some(self.attach_execution_control(actual).await?),
                    None => None,
                };
                Ok(TaskBusinessResponse::Status(json!({"actual":actual})))
            }
            (TaskBusinessWire::Resolve(wire), TaskBusinessChallenge::Resolve(challenge)) => {
                let proof = ExecutionHandoffResolutionFreshness {
                    challenge,
                    receipts,
                };
                let auth = {
                    let store = self.store.lock().await;
                    verification.run(|| {
                        store.execution_handoff_resolution_authorization_scoped(
                            &view,
                            &wire,
                            &evidence,
                            now_ms() as u64,
                        )
                    })?
                };
                if wire.resolution.purpose == ExecutionResolutionPurpose::ReconcileTerminal {
                    let store = self.store.lock().await;
                    let result = verification.run(|| {
                        store.authorize_execution_handoff_terminal_reconciliation_scoped(
                            &view,
                            &auth,
                            &proof,
                            now_ms() as u64,
                        )
                    })?;
                    return Ok(TaskBusinessResponse::Business(serde_json::to_value(
                        result,
                    )?));
                }
                {
                    let store = self.store.lock().await;
                    verification.run(|| {
                        store.authorize_execution_handoff_resolution_fence_scoped(
                            &view,
                            &auth,
                            &proof,
                            now_ms() as u64,
                        )
                    })?;
                }
                let source = wire
                    .resolution
                    .observation
                    .handoff
                    .handoff
                    .source_statement()?;
                let context = ExecutionContext {
                    space_id: source.project.space,
                    resource_id: source.project.resource,
                    operation_id: source.operation_id,
                    shared: true,
                    lease_expires_at_ms: proof.challenge.expires_at_ms,
                };
                let command =
                    super::handoff::handoff_is_command(&wire.resolution.observation.handoff)?;
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
                    &verification,
                )
                .await?;
                let (stopped, _) = stopped?;
                self.engine.complete_fenced_operation(&stopped).await?;
                let fresh = self
                    .task_business_resolution_freshness(session, &wire, &evidence)
                    .await?;
                let mut store = self.store.lock().await;
                let result = verification.run(|| {
                    store.resolve_execution_handoff_scoped(
                        &view,
                        &self.identity,
                        &wire,
                        &fresh,
                        &evidence,
                        &stopped,
                        now_ms() as u64,
                    )
                })?;
                Ok(TaskBusinessResponse::Business(serde_json::to_value(
                    result,
                )?))
            }
            _ => bail!("Task business receipts changed their typed challenge"),
        }
    }

    async fn task_business_resolution_freshness(
        &self,
        session: &TaskBusinessPolicySession,
        wire: &vibeface_secure::authorization::execution_handoff::SignedExecutionHandoffResolution,
        evidence: &TransportEvidence,
    ) -> Result<ExecutionHandoffResolutionFreshness> {
        let verification = SignatureVerification::new();
        let typed = TaskBusinessWire::Resolve(wire.clone());
        let challenge = {
            let mut store = self.store.lock().await;
            verification.run(|| {
                let now = now_ms() as u64;
                let view = store.task_business_policy_view(&self.identity, session, &typed, now)?;
                store
                    .issue_execution_handoff_resolution_challenge_scoped(&view, wire, evidence, now)
            })?
        };
        let now = now_ms() as u64;
        let signer = vibeface_secure::Identity::from_secret_hex(&self.identity.secret_key_hex)
            .map_err(anyhow::Error::msg)?;
        let request = Signed::sign(
            TaskBusinessReceiptRead {
                version: AUTH_VERSION,
                source_header_hash: session.source().header().hash()?,
                business_wire_hash: typed.hash()?,
                challenge: TaskBusinessChallenge::Resolve(challenge.clone()),
                acceptance: None,
                nonce: Nonce::random()?,
                issued_at_ms: now,
                expires_at_ms: challenge.expires_at_ms.min(
                    now.checked_add(INVOCATION_MAX_AGE_MS)
                        .context("receipt read expiry overflow")?,
                ),
            },
            &signer,
        )?;
        let mut client = crate::client::TaskBusinessReceiptClient::connect(
            &self.network,
            &self.identity,
            session,
        )
        .await?;
        let mut receipts = client.read(&request).await?;
        let store = self.store.lock().await;
        verification.run(|| {
            let now = now_ms() as u64;
            let view = store.task_business_policy_view(&self.identity, session, &typed, now)?;
            receipts.extend(store.sign_execution_handoff_resolution_receipts_scoped(
                &view,
                wire,
                evidence,
                &challenge,
                &self.identity,
                now,
            )?);
            for receipt in &receipts {
                receipt.payload.validate_currents()?;
            }
            receipts.sort_by(|left, right| left.payload.currents.cmp(&right.payload.currents));
            Ok(ExecutionHandoffResolutionFreshness {
                challenge,
                receipts,
            })
        })
    }

    pub(crate) async fn task_business_receipt_read(
        self: &Arc<Self>,
        peer: &ActualPolicyNoisePeer,
        request: Signed<TaskBusinessReceiptRead>,
    ) -> Result<Vec<Signed<FreshnessReceipt>>> {
        let verification = SignatureVerification::new();
        let store = self.store.lock().await;
        verification.run(|| {
            let now = now_ms() as u64;
            let (_, wire) = store.task_business_source_originals(
                &request.payload.source_header_hash,
                &request.payload.business_wire_hash,
            )?;
            match &wire {
                TaskBusinessWire::Submit(original) if request.payload.acceptance.is_some() => {
                    let scoped = store.current_task_business_operation_source(&self.identity, original, now)?;
                    let current = scoped.task_policy().context("NeedPolicySync: operation has no current Task policy")?;
                    store.read_task_business_operation_receipts(
                        &self.identity, current, peer, &request, now,
                    )
                }
                TaskBusinessWire::Submit(original) => {
                    let (scoped, business) = store.current_task_business_submit(&self.identity, original, now)?;
                    let current = scoped.task_policy().context("NeedPolicySync: submission has no current Task policy")?;
                    store.read_task_business_receipts(
                        &self.identity, current,
                        vibeface_secure::authorization::task_business_policy::TaskBusinessAuthorization::Submit(&business),
                        peer, &request, now,
                    )
                }
                TaskBusinessWire::Observe(original) => {
                    let (current, business) =
                        store.current_task_business_observe(&self.identity, original, now)?;
                    store.read_task_business_receipts(
                        &self.identity,
                        &current,
                        vibeface_secure::authorization::task_business_policy::TaskBusinessAuthorization::Observe(&business),
                        peer,
                        &request,
                        now,
                    )
                }
                TaskBusinessWire::Resolve(original) => {
                    let (current, business) =
                        store.current_task_business_resolve(&self.identity, original, now)?;
                    store.read_task_business_receipts(
                        &self.identity,
                        &current,
                        vibeface_secure::authorization::task_business_policy::TaskBusinessAuthorization::Resolve(&business),
                        peer,
                        &request,
                        now,
                    )
                }
            }
        })
    }

    async fn spawn_task_business_handoff(
        self: &Arc<Self>,
        session: &TaskBusinessPolicySession,
        wire: vibeface_secure::authorization::execution_handoff::SignedExecutionHandoff,
        accepted: crate::identity::execution_handoff::AdmittedExecutionHandoff,
    ) -> Result<()> {
        let (acceptance, mut recovery) = {
            let store = self.store.lock().await;
            let (stored, acceptance) = store.execution_handoff_original_acceptance(
                wire.handoff.execution_resource.space.as_str(),
                accepted.execution_context.space_id.as_str(),
                &accepted.original_request.operation_id,
            )?;
            ensure!(
                stored == wire,
                "accepted Task business changed its original wire"
            );
            let recovery = store.begin_task_business_original_recovery(
                &self.identity,
                &TaskBusinessWire::Submit(wire),
                &acceptance,
                now_ms() as u64,
            )?;
            (acceptance, recovery)
        };
        let node = self.clone();
        let session = session.clone();
        tokio::spawn(async move {
            let run = async {
                loop {
                    let source = {
                        let store = node.store.lock().await;
                        store.advance_task_business_original_recovery(&mut recovery)?
                    };
                    if let Some(source) = source {
                        return node
                            .execute_handoff_with_business(accepted, session, source, acceptance)
                            .await;
                    }
                    tokio::task::yield_now().await;
                }
            }
            .await;
            if let Err(error) = run {
                tracing::warn!(error=%error,"accepted Task business requires reconciliation");
            }
        });
        Ok(())
    }
}
