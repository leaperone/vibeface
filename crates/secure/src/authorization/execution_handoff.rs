use super::{events::*, task_binding::*, *};
use serde_json::Value;

pub const MAX_EXECUTION_HANDOFF_BYTES: usize = 8 * 1024 * 1024;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct CompactSdkOrigin {
    pub message_event: EventRef,
    pub sdk_trigger: EventRef,
    pub invocation: Signed<Invocation>,
    pub challenge: FreshnessChallenge,
    pub receipts: Vec<Signed<FreshnessReceipt>>,
    pub admitted_at_ms: u64,
    pub transport: StoredTransport,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ProjectExecutionSource {
    pub version: u16,
    pub capsule_hash: ObjectHash,
    pub project: ResourceRef,
    pub task: EventRef,
    pub operation_id: String,
    pub original_invocation_hash: ObjectHash,
    pub original_admission_hash: ObjectHash,
    pub binding_hash: String,
    pub binding_generation: u64,
    pub execution_session: SessionRef,
    pub sdk_origin_hash: Option<ObjectHash>,
    pub task_sdk_origin_hash: Option<ObjectHash>,
}
impl sealed::Sealed for ProjectExecutionSource {}
impl SignedObject for ProjectExecutionSource {
    const DOMAIN: &'static str = "project-execution-source-v2";
}

pub struct VerifiedProjectExecutionSource {
    capsule_hash: ObjectHash,
    usage: ExecutionProofUsage,
}
impl VerifiedProjectExecutionSource {
    pub fn capsule_hash(&self) -> &ObjectHash {
        &self.capsule_hash
    }
    pub fn proof_usage(&self) -> &ExecutionProofUsage {
        &self.usage
    }
    pub fn original_admission(
        &self,
        handoff: &ExecutionHandoff,
        now_ms: u64,
    ) -> Result<VerifiedAdmission> {
        self.check_capsule(handoff)?;
        let (_, proof, _, _) = original_dispatch(handoff)?;
        attested_admission(
            &proof.invocation,
            &proof.challenge,
            proof.admitted_at_ms,
            now_ms,
        )
    }
    pub fn sdk_parent_admission(
        &self,
        handoff: &ExecutionHandoff,
        now_ms: u64,
    ) -> Result<Option<VerifiedAdmission>> {
        self.check_capsule(handoff)?;
        let Some(origin) = &handoff.sdk_origin else {
            return Ok(None);
        };
        let (_, proof, _, _) = original_dispatch(handoff)?;
        let parent = &origin.invocation.payload;
        require(
            matches!(parent.actor, Actor::Account(_))
                && parent.initiator == parent.actor
                && parent.initiating_grant.is_none()
                && parent.action == Action::MessageAppend
                && parent.resource == proof.invocation.payload.resource
                && parent.audience == proof.invocation.payload.audience
                && parent.context.conversation == proof.invocation.payload.context.conversation
                && parent.context.workspace_resource
                    == proof.invocation.payload.context.workspace_resource
                && parent.context.sdk_session.is_none()
                && parent.context.execution_session.is_none()
                && origin.admitted_at_ms <= proof.admitted_at_ms,
            "invalid attested Account message origin",
        )?;
        Ok(Some(attested_admission(
            &origin.invocation,
            &origin.challenge,
            origin.admitted_at_ms,
            now_ms,
        )?))
    }
    fn check_capsule(&self, handoff: &ExecutionHandoff) -> Result<()> {
        require(
            handoff.source_statement()?.capsule_hash == self.capsule_hash,
            "Project source checkpoint belongs to another capsule",
        )
    }
}

fn attested_admission(
    invocation: &Signed<Invocation>,
    challenge: &FreshnessChallenge,
    admitted_at_ms: u64,
    now_ms: u64,
) -> Result<VerifiedAdmission> {
    let value = &invocation.payload;
    require(
        admitted_at_ms <= now_ms
            && value.not_before_ms <= admitted_at_ms
            && admitted_at_ms < value.expires_at_ms
            && challenge.version == AUTH_VERSION
            && challenge.stage == CheckStage::Admit
            && challenge.invocation_hash == invocation.hash()?
            && challenge.operation_id == value.operation_id
            && challenge.body_hash == value.body_hash
            && challenge.audience == value.audience
            && challenge.presenter == value.presenter
            && challenge.issued_at_ms <= admitted_at_ms
            && admitted_at_ms < challenge.expires_at_ms
            && challenge.policies.len() <= MAX_POLICY_DOMAINS,
        "invalid Project-attested admission checkpoint",
    )?;
    Ok(VerifiedAdmission {
        invocation: invocation.clone(),
        admitted_at_ms,
    })
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ExecutionHandoff {
    pub version: u16,
    pub project_dispatch: SignedEvent,
    pub task_creation: SignedEvent,
    pub sdk_origin: Option<CompactSdkOrigin>,
    pub task_sdk_origin: Option<CompactSdkOrigin>,
    pub execution_resource: ResourceRef,
    pub execution_audience: Audience,
    pub target_public_key: PublicKey,
    pub source_attestation: Option<Signed<ProjectExecutionSource>>,
}

impl ExecutionHandoff {
    pub fn source_statement(&self) -> Result<ProjectExecutionSource> {
        let (event, proof, _, binding) = original_dispatch(self)?;
        #[derive(Serialize)]
        struct Capsule<'a> {
            version: u16,
            project_dispatch: &'a SignedEvent,
            task_creation: &'a SignedEvent,
            sdk_origin: &'a Option<CompactSdkOrigin>,
            task_sdk_origin: &'a Option<CompactSdkOrigin>,
            execution_resource: &'a ResourceRef,
            execution_audience: &'a Audience,
            target_public_key: &'a PublicKey,
        }
        Ok(ProjectExecutionSource {
            version: AUTH_VERSION,
            capsule_hash: ObjectHash::body(&Capsule {
                version: self.version,
                project_dispatch: &self.project_dispatch,
                task_creation: &self.task_creation,
                sdk_origin: &self.sdk_origin,
                task_sdk_origin: &self.task_sdk_origin,
                execution_resource: &self.execution_resource,
                execution_audience: &self.execution_audience,
                target_public_key: &self.target_public_key,
            })?,
            project: ResourceRef {
                space: SpaceId::parse(&event.network_id)?,
                resource: event.resource_id,
            },
            task: EventRef::parse(&self.task_creation.hash)?,
            operation_id: proof.invocation.payload.operation_id.clone(),
            original_invocation_hash: proof.invocation.hash()?,
            original_admission_hash: ObjectHash::body(&proof)?,
            binding_hash: binding_hash(&binding.binding).map_err(invalid_shape)?,
            binding_generation: binding.binding.generation,
            execution_session: SessionRef {
                target: self.execution_audience.device.clone(),
                session_id: binding.binding.session_id,
            },
            sdk_origin_hash: self.sdk_origin.as_ref().map(ObjectHash::body).transpose()?,
            task_sdk_origin_hash: self
                .task_sdk_origin
                .as_ref()
                .map(ObjectHash::body)
                .transpose()?,
        })
    }

    pub fn attest_source(&mut self, identity: &Identity) -> Result<()> {
        self.source_attestation = Some(Signed::sign(self.source_statement()?, identity)?);
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct SignedExecutionHandoff {
    pub handoff: ExecutionHandoff,
    pub invocation: Signed<Invocation>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ExecutionHandoffResult {
    pub handoff: SignedExecutionHandoff,
    pub receipt: Signed<ExecutionHandoffReceipt>,
    pub result: Value,
}

impl SignedExecutionHandoff {
    pub fn hash(&self) -> Result<ObjectHash> {
        require(
            canonical(self)?.len() <= MAX_EXECUTION_HANDOFF_BYTES,
            "execution handoff is too large",
        )?;
        ObjectHash::body(self)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ExecutionHandoffChallenge {
    pub version: u16,
    pub stage: CheckStage,
    pub handoff_hash: ObjectHash,
    pub original_invocation_hash: ObjectHash,
    pub submission_invocation_hash: ObjectHash,
    pub original_body_hash: ObjectHash,
    pub original_request_hash: ObjectHash,
    pub original_resource: ResourceRef,
    pub execution_resource: ResourceRef,
    pub execution_audience: Audience,
    pub target_public_key: PublicKey,
    pub project_presenter: DeviceId,
    pub operation_id: String,
    pub binding_hash: String,
    pub binding_generation: u64,
    pub execution_session: SessionRef,
    pub policies: Vec<PolicyStamp>,
    pub nonce: Nonce,
    pub issued_at_ms: u64,
    pub expires_at_ms: u64,
}
impl sealed::Sealed for ExecutionHandoffChallenge {}
impl SignedObject for ExecutionHandoffChallenge {
    const DOMAIN: &'static str = "execution-handoff-challenge-v2";
}

impl ExecutionHandoffChallenge {
    pub fn new(
        handoff: &VerifiedExecutionHandoff,
        stage: CheckStage,
        nonce: Nonce,
        issued_at_ms: u64,
        expires_at_ms: u64,
    ) -> Result<Self> {
        require(
            issued_at_ms >= handoff.verified_at_ms,
            "handoff challenge clock moved backwards",
        )?;
        Self::bound_to(handoff, stage, nonce, issued_at_ms, expires_at_ms)
    }

    fn bound_to(
        handoff: &VerifiedExecutionHandoff,
        stage: CheckStage,
        nonce: Nonce,
        issued_at_ms: u64,
        expires_at_ms: u64,
    ) -> Result<Self> {
        require(
            (handoff.submission.phase == AuthorizationPhase::Admission
                && stage == CheckStage::Admit)
                || (handoff.submission.phase == AuthorizationPhase::Operation
                    && matches!(
                        stage,
                        CheckStage::Start | CheckStage::Renew | CheckStage::RemoteRead
                    )),
            "handoff stage does not match admission state",
        )?;
        validate_window(issued_at_ms, expires_at_ms, issued_at_ms)?;
        require(
            expires_at_ms - issued_at_ms <= EXECUTION_LEASE_MS,
            "invalid execution handoff challenge lifetime",
        )?;
        Ok(Self {
            version: AUTH_VERSION,
            stage,
            handoff_hash: handoff.hash.clone(),
            original_invocation_hash: handoff.original.invocation_hash().clone(),
            submission_invocation_hash: handoff.submission.invocation_hash().clone(),
            original_body_hash: handoff.original.body_hash().clone(),
            original_request_hash: handoff.original_request_hash.clone(),
            original_resource: handoff.original.resource().clone(),
            execution_resource: handoff.wire.handoff.execution_resource.clone(),
            execution_audience: handoff.wire.handoff.execution_audience.clone(),
            target_public_key: handoff.wire.handoff.target_public_key.clone(),
            project_presenter: handoff.submission.device().clone(),
            operation_id: handoff.original.operation_id().to_owned(),
            binding_hash: handoff.binding_hash.clone(),
            binding_generation: handoff.binding.binding.generation,
            execution_session: handoff
                .submission
                .signed_invocation()
                .payload
                .context
                .execution_session
                .clone()
                .ok_or(AuthorizationError::InvalidProof(
                    "handoff is missing its execution session",
                ))?,
            policies: handoff.dependencies.clone(),
            nonce,
            issued_at_ms,
            expires_at_ms,
        })
    }

    pub fn hash(&self) -> Result<ObjectHash> {
        Ok(ObjectHash(format!(
            "h2_{}",
            digest(Self::DOMAIN, &canonical(self)?)
        )))
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ExecutionHandoffFreshness {
    pub challenge: ExecutionHandoffChallenge,
    pub receipts: Vec<Signed<FreshnessReceipt>>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ExecutionHandoffAdmissionReceipt {
    pub version: u16,
    pub handoff_hash: ObjectHash,
    pub submission_invocation_hash: ObjectHash,
    pub original_invocation_hash: ObjectHash,
    pub original_request_hash: ObjectHash,
    pub admission_hash: ObjectHash,
    pub transport_hash: ObjectHash,
    pub admitted_at_ms: u64,
}
impl sealed::Sealed for ExecutionHandoffAdmissionReceipt {}
impl SignedObject for ExecutionHandoffAdmissionReceipt {
    const DOMAIN: &'static str = "execution-handoff-admission-receipt-v2";
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ExecutionHandoffLeaseReceipt {
    pub version: u16,
    pub admission_receipt_hash: ObjectHash,
    pub handoff_hash: ObjectHash,
    pub freshness_hash: ObjectHash,
    pub verified_at_ms: u64,
    pub deadline_ms: u64,
}
impl sealed::Sealed for ExecutionHandoffLeaseReceipt {}
impl SignedObject for ExecutionHandoffLeaseReceipt {
    const DOMAIN: &'static str = "execution-handoff-lease-receipt-v2";
}

impl ExecutionHandoffLeaseReceipt {
    pub fn new(
        wire: &SignedExecutionHandoff,
        admission: &Signed<ExecutionHandoffAdmissionReceipt>,
        proof: &ExecutionHandoffFreshness,
        verified_at_ms: u64,
        deadline_ms: u64,
    ) -> Result<Self> {
        require(
            matches!(proof.challenge.stage, CheckStage::Start | CheckStage::Renew)
                && admission.payload.admitted_at_ms <= verified_at_ms
                && proof.challenge.issued_at_ms <= verified_at_ms
                && verified_at_ms < deadline_ms
                && proof.receipts.len() <= MAX_POLICY_DOMAINS
                && proof.challenge.policies.len() <= MAX_POLICY_DOMAINS
                && deadline_ms
                    == proof
                        .receipts
                        .iter()
                        .map(|receipt| receipt.payload.expires_at_ms)
                        .fold(proof.challenge.expires_at_ms, u64::min),
            "invalid executor lease checkpoint interval",
        )?;
        Ok(Self {
            version: AUTH_VERSION,
            admission_receipt_hash: admission.hash()?,
            handoff_hash: wire.hash()?,
            freshness_hash: ObjectHash::body(proof)?,
            verified_at_ms,
            deadline_ms,
        })
    }
}

pub fn verify_execution_handoff_lease_checkpoint(
    wire: &SignedExecutionHandoff,
    admission: &Signed<ExecutionHandoffAdmissionReceipt>,
    proof: &ExecutionHandoffFreshness,
    verified_at_ms: u64,
    deadline_ms: u64,
    receipt: &Signed<ExecutionHandoffLeaseReceipt>,
) -> Result<ExecutionProofUsage> {
    receipt.verify_signature()?;
    require(
        receipt.issuer == wire.handoff.target_public_key
            && receipt.payload
                == ExecutionHandoffLeaseReceipt::new(
                    wire,
                    admission,
                    proof,
                    verified_at_ms,
                    deadline_ms,
                )?,
        "executor lease checkpoint changed",
    )?;
    let mut usage = ExecutionProofUsage::default();
    usage.record_signed(receipt)?;
    usage.record_metadata(proof)?;
    for receipt in &proof.receipts {
        usage
            .historical_wrappers
            .insert(receipt.hash()?, canonical(receipt)?.len());
    }
    usage.enforce_limits()?;
    Ok(usage)
}

impl ExecutionHandoffAdmissionReceipt {
    pub fn new(
        wire: &SignedExecutionHandoff,
        admission: &ExecutionHandoffFreshness,
        transport: &StoredTransport,
        admitted_at_ms: u64,
    ) -> Result<Self> {
        let (_, original, request, _) = original_dispatch(&wire.handoff)?;
        Ok(Self {
            version: AUTH_VERSION,
            handoff_hash: wire.hash()?,
            submission_invocation_hash: wire.invocation.hash()?,
            original_invocation_hash: original.invocation.hash()?,
            original_request_hash: ObjectHash::body(&request)?,
            admission_hash: ObjectHash::body(admission)?,
            transport_hash: ObjectHash::body(transport)?,
            admitted_at_ms,
        })
    }
}

pub fn verify_execution_handoff_admission_checkpoint(
    wire: &SignedExecutionHandoff,
    receipt: &Signed<ExecutionHandoffAdmissionReceipt>,
    admission: &ExecutionHandoffFreshness,
    transport: &StoredTransport,
    admitted_at_ms: u64,
) -> Result<VerifiedExecutionAdmission> {
    receipt.verify_signature()?;
    require(
        receipt.issuer == wire.handoff.target_public_key
            && receipt.payload
                == ExecutionHandoffAdmissionReceipt::new(
                    wire,
                    admission,
                    transport,
                    admitted_at_ms,
                )?
            && admission.challenge.stage == CheckStage::Admit
            && admission.challenge.issued_at_ms <= admitted_at_ms
            && admitted_at_ms < admission.challenge.expires_at_ms
            && wire.invocation.payload.not_before_ms <= admitted_at_ms
            && admitted_at_ms < wire.invocation.payload.expires_at_ms,
        "executor acceptance checkpoint changed",
    )?;
    let mut usage = ExecutionProofUsage::default();
    usage.record_signed(receipt)?;
    usage.record_metadata(admission)?;
    usage.record_metadata(transport)?;
    usage.enforce_limits()?;
    Ok(VerifiedExecutionAdmission {
        handoff_hash: wire.hash()?,
        submission: VerifiedAdmission {
            invocation: wire.invocation.clone(),
            admitted_at_ms,
        },
        usage,
    })
}

#[derive(Clone, Debug)]
pub struct VerifiedExecutionHandoff {
    wire: SignedExecutionHandoff,
    hash: ObjectHash,
    original: VerifiedAuthorization,
    submission: VerifiedAuthorization,
    original_request_hash: ObjectHash,
    binding: SignedBinding,
    binding_hash: String,
    dependencies: Vec<PolicyStamp>,
    verified_at_ms: u64,
    usage: ExecutionProofUsage,
}

impl VerifiedExecutionHandoff {
    pub fn proof_usage(&self) -> &ExecutionProofUsage {
        &self.usage
    }
    pub fn wire(&self) -> &SignedExecutionHandoff {
        &self.wire
    }
    pub fn hash(&self) -> &ObjectHash {
        &self.hash
    }
    pub fn original(&self) -> &VerifiedAuthorization {
        &self.original
    }
    pub fn submission(&self) -> &VerifiedAuthorization {
        &self.submission
    }
    pub fn binding(&self) -> &SignedBinding {
        &self.binding
    }
    pub fn dependencies(&self) -> &[PolicyStamp] {
        &self.dependencies
    }
    pub fn expires_at_ms(&self) -> u64 {
        self.original
            .expires_at_ms()
            .min(self.submission.expires_at_ms())
    }
}

#[derive(Clone, Debug)]
pub struct VerifiedExecutionAdmission {
    handoff_hash: ObjectHash,
    submission: VerifiedAdmission,
    usage: ExecutionProofUsage,
}
impl VerifiedExecutionAdmission {
    pub fn proof_usage(&self) -> &ExecutionProofUsage {
        &self.usage
    }
}

#[derive(Clone, Debug)]
pub struct VerifiedExecutionFreshness {
    handoff_hash: ObjectHash,
    challenge_hash: ObjectHash,
    nonce: Nonce,
    stage: CheckStage,
    dependencies: Vec<PolicyStamp>,
    verified_at_ms: u64,
    expires_at_ms: u64,
    usage: ExecutionProofUsage,
}

impl VerifiedExecutionFreshness {
    pub fn proof_usage(&self) -> &ExecutionProofUsage {
        &self.usage
    }
    pub fn expires_at_ms(&self) -> u64 {
        self.expires_at_ms
    }
    pub fn challenge_hash(&self) -> &ObjectHash {
        &self.challenge_hash
    }
    pub fn nonce(&self) -> &Nonce {
        &self.nonce
    }
    pub fn stage(&self) -> CheckStage {
        self.stage
    }
}

fn invalid_shape(_: impl std::fmt::Display) -> AuthorizationError {
    AuthorizationError::InvalidProof("invalid execution handoff business evidence")
}

fn original_dispatch(
    handoff: &ExecutionHandoff,
) -> Result<(Event, AdmissionProof, Value, SignedBinding)> {
    require(
        handoff.version == AUTH_VERSION,
        "invalid execution handoff version",
    )?;
    let event: Event =
        serde_json::from_str(&handoff.project_dispatch.canonical_body).map_err(invalid_shape)?;
    let EventCause::Invocation { admission } = &event.cause else {
        return Err(AuthorizationError::InvalidProof(
            "handoff requires an admitted dispatch",
        ));
    };
    let EventBody::DispatchRecorded {
        operation_id,
        task_id,
        target_node,
        session_id,
        action,
    } = &event.body
    else {
        return Err(AuthorizationError::InvalidProof(
            "handoff source is not a dispatch",
        ));
    };
    let source = &admission.source_body;
    let request = source
        .get("request")
        .ok_or(AuthorizationError::InvalidProof("missing original request"))?;
    require(
        source["method"] == "operate"
            && source["space_id"].as_str() == Some(event.network_id.as_str())
            && source["resource_id"].as_str() == Some(event.resource_id.as_str())
            && source["authorization_id"].as_str() == Some(operation_id.as_str())
            && request == action
            && request["space_id"].as_str() == Some(event.network_id.as_str())
            && request["conversation_id"].as_str() == Some(event.scope_id.as_str())
            && request["operation_id"].as_str() == Some(operation_id.as_str())
            && request["target_node"].as_str() == Some(target_node.as_str())
            && request["session_id"].as_str() == Some(session_id.as_str())
            && request.get("task_id")
                == Some(&serde_json::to_value(task_id).map_err(invalid_shape)?),
        "handoff dispatch differs from the original signed request",
    )?;
    let binding: SignedBinding =
        serde_json::from_value(request["binding"].clone()).map_err(invalid_shape)?;
    validate_binding(&binding.binding).map_err(invalid_shape)?;
    let b = &binding.binding;
    require(
        b.network_id == event.network_id
            && b.conversation_id == event.scope_id
            && Some(b.task_id.as_str()) == task_id.as_deref()
            && b.target_node == *target_node
            && b.session_id == *session_id,
        "handoff binding does not match its original operation",
    )?;
    Ok((event.clone(), admission.clone(), request.clone(), binding))
}

pub fn execution_submission_facts(
    handoff: &ExecutionHandoff,
    original: &VerifiedAuthorization,
) -> Result<RequestFacts> {
    let (event, proof, request, binding) = original_dispatch(handoff)?;
    require(
        proof.invocation == *original.signed_invocation()
            && original.phase == AuthorizationPhase::Operation
            && original.action() == execution_action(&request)?
            && *original.body_hash() == ObjectHash::body(&proof.source_body)?
            && original.resource().space.as_str() == event.network_id
            && original.resource().resource == event.resource_id
            && request["operation_id"].as_str() == Some(original.operation_id())
            && original
                .signed_invocation()
                .payload
                .context
                .task
                .as_ref()
                .is_some_and(|task| task.as_str() == binding.binding.task_ref),
        "handoff has a different original authorization",
    )?;
    let mut context = original.signed_invocation().payload.context.clone();
    require(
        context.execution_session.as_ref().is_some_and(|session| {
            session.target == handoff.execution_audience.device
                && session.session_id == binding.binding.session_id
        }) && DeviceId::from_key(&handoff.target_public_key) == handoff.execution_audience.device,
        "handoff execution target differs from the original invocation",
    )?;
    context.sdk_session = None;
    context.workspace_resource = Some(handoff.execution_resource.resource.clone());
    Ok(RequestFacts {
        resource: handoff.execution_resource.clone(),
        action: Action::ExecutionSubmit,
        audience: handoff.execution_audience.clone(),
        operation_id: original.operation_id().to_owned(),
        body_hash: ObjectHash::body(handoff)?,
        context,
    })
}

fn execution_action(request: &Value) -> Result<Action> {
    Ok(match request["action"]["type"].as_str() {
        Some("open") => Action::ExecutionOpen,
        Some("read") => Action::OperationStatusRead,
        Some("send") => Action::ExecutionSend,
        Some("steer") => Action::ExecutionSteer,
        Some("interrupt") => Action::ExecutionInterrupt,
        Some("respond_request") => Action::ExecutionRespondApproval,
        _ => {
            return Err(AuthorizationError::InvalidProof(
                "unsupported handoff execution action",
            ));
        }
    })
}

impl VerifiedPolicySet {
    pub fn verify_project_execution_source(
        &self,
        handoff: &ExecutionHandoff,
        now_ms: u64,
    ) -> Result<VerifiedProjectExecutionSource> {
        let mut verified = self.verify_execution_observation_source(handoff, now_ms)?;
        let (_, proof, _, _) = original_dispatch(handoff)?;
        if let Some(parent) = verified.sdk_parent_admission(handoff, now_ms)? {
            let origin = handoff
                .sdk_origin
                .as_ref()
                .ok_or(AuthorizationError::InvalidProof("missing SDK origin"))?;
            let invocation = &proof.invocation.payload;
            let Actor::Service(service) = &invocation.actor else {
                return Err(AuthorizationError::InvalidProof(
                    "SDK origin has no Service child",
                ));
            };
            let session =
                invocation
                    .context
                    .sdk_session
                    .as_ref()
                    .ok_or(AuthorizationError::InvalidProof(
                        "SDK origin has no session",
                    ))?;
            require(
                session.target == invocation.device
                    && session.session_id == origin.sdk_trigger.as_str()
                    && proof.source_body["parent_invocation"].as_str()
                        == Some(origin.invocation.hash()?.as_str())
                    && proof.source_body["sdk_session"]
                        == serde_json::to_value(session).map_err(invalid_shape)?
                    && proof.transport.clone().into_evidence()
                        == TransportEvidence::LocalAgentAdapter {
                            service: service.clone(),
                            session: session.clone(),
                        },
                "SDK checkpoint changed its Account source or adapter session",
            )?;
            let parent_value = &origin.invocation.payload;
            let parent_facts = RequestFacts {
                resource: parent_value.resource.clone(),
                action: parent_value.action,
                audience: parent_value.audience.clone(),
                operation_id: parent_value.operation_id.clone(),
                body_hash: parent_value.body_hash.clone(),
                context: parent_value.context.clone(),
            };
            let parent = self.verify_operation_authorization(
                &parent,
                &parent_facts,
                &origin.transport.clone().into_evidence(),
                now_ms,
            )?;
            let child_facts = RequestFacts {
                resource: invocation.resource.clone(),
                action: invocation.action,
                audience: invocation.audience.clone(),
                operation_id: invocation.operation_id.clone(),
                body_hash: invocation.body_hash.clone(),
                context: invocation.context.clone(),
            };
            let delegation = self.verify_service_delegation(
                &parent,
                service,
                &invocation.grant_leaf,
                &child_facts,
                now_ms,
            )?;
            require(
                delegation.device() == &invocation.device
                    && delegation.initiator() == &invocation.initiator
                    && delegation.initiating_grant() == invocation.initiating_grant.as_ref(),
                "SDK checkpoint changed its current delegation",
            )?;
            verified.usage.record_authorization(&parent)?;
        }
        Ok(verified)
    }

    pub fn verify_execution_observation_source(
        &self,
        handoff: &ExecutionHandoff,
        now_ms: u64,
    ) -> Result<VerifiedProjectExecutionSource> {
        let mut capsule_usage = ExecutionProofUsage::default();
        capsule_usage.record_capsule(handoff)?;
        capsule_usage.enforce_limits()?;
        let (dispatch, proof, _, binding) = original_dispatch(handoff)?;
        let statement = handoff.source_statement()?;
        let signed =
            handoff
                .source_attestation
                .as_ref()
                .ok_or(AuthorizationError::InvalidProof(
                    "missing Project execution source attestation",
                ))?;
        signed.verify_signature()?;
        require(
            signed.payload == statement,
            "Project execution source attestation changed",
        )?;
        let audience = &proof.invocation.payload.audience;
        let project = self
            .resource(&statement.project.space, &statement.project.resource)
            .ok_or(AuthorizationError::InvalidProof("missing source Project"))?;
        let certificate = self.find_device(&audience.device)?;
        require(
            project.payload.kind == ResourceKind::Project
                && project.payload.host == audience.service
                && certificate.payload.subject == Actor::Service(project.payload.host.clone())
                && certificate
                    .payload
                    .uses
                    .contains(&DeviceUse::ServiceInvocation)
                && signed.issuer == certificate.payload.public_key
                && proof.invocation.payload.resource == statement.project,
            "capsule is not attested by its current Project host",
        )?;
        let mut source_budget = Budget::default();
        self.validate_certificate(
            certificate,
            now_ms,
            &mut source_budget,
            &mut BTreeMap::new(),
            &mut BTreeSet::new(),
        )?;
        let task = events::verify_signed_event(
            &serde_json::to_string(&handoff.task_creation).map_err(invalid_shape)?,
            statement.project.space.as_str(),
            statement.project.resource.as_str(),
            certificate.payload.public_key.as_str(),
        )
        .map_err(invalid_shape)?;
        events::verify_signed_event(
            &serde_json::to_string(&handoff.project_dispatch).map_err(invalid_shape)?,
            statement.project.space.as_str(),
            statement.project.resource.as_str(),
            certificate.payload.public_key.as_str(),
        )
        .map_err(invalid_shape)?;
        let EventBody::TaskCreated { task_id, .. } = &task.event().body else {
            return Err(AuthorizationError::InvalidProof(
                "capsule source is not a task creation",
            ));
        };
        require(
            binding.binding.task_ref == handoff.task_creation.hash
                && binding.binding.task_id == *task_id
                && binding.binding.authority_node == task.event().author
                && task.event().scope_id == dispatch.scope_id
                && task.event().blobs.is_empty()
                && dispatch.blobs.is_empty(),
            "execution capsule belongs to another task",
        )?;
        verify_binding(&binding, certificate.payload.public_key.as_str()).map_err(invalid_shape)?;
        let mut usage = ExecutionProofUsage::default();
        usage.record_budget(&source_budget)?;
        usage.record_signed(signed)?;
        let EventCause::Invocation {
            admission: task_admission,
        } = &task.event().cause
        else {
            return Err(AuthorizationError::InvalidProof(
                "Task source has no admission",
            ));
        };
        usage.record_event_signature(&handoff.task_creation, task.event(), task_admission)?;
        usage.record_event_signature(&handoff.project_dispatch, &dispatch, &proof)?;
        usage.record_binding(&binding)?;
        let verified = VerifiedProjectExecutionSource {
            capsule_hash: statement.capsule_hash,
            usage,
        };
        verified.original_admission(handoff, now_ms)?;
        Ok(verified)
    }

    pub fn verify_execution_handoff(
        &self,
        wire: &SignedExecutionHandoff,
        original: &VerifiedAuthorization,
        transport: &TransportEvidence,
        admitted: Option<&VerifiedExecutionAdmission>,
        now_ms: u64,
    ) -> Result<VerifiedExecutionHandoff> {
        self.check_current(original, now_ms)?;
        let source = self.verify_project_execution_source(&wire.handoff, now_ms)?;
        let hash = wire.hash()?;
        let facts = execution_submission_facts(&wire.handoff, original)?;
        let (event, original_proof, request, binding) = original_dispatch(&wire.handoff)?;
        require(
            original_proof.source_body["execution_grant"].as_str()
                == Some(wire.invocation.payload.grant_leaf.as_str()),
            "submission changed the originally selected execution grant",
        )?;
        let project = self
            .resource(&original.resource().space, &original.resource().resource)
            .ok_or(AuthorizationError::InvalidProof("missing Project resource"))?;
        let execution = self
            .resource(&facts.resource.space, &facts.resource.resource)
            .ok_or(AuthorizationError::InvalidProof(
                "missing ExecutionService resource",
            ))?;
        let project_device = self.find_device(&original.audience().device)?;
        let target = self.find_device(&facts.audience.device)?;
        require(
            project.payload.kind == ResourceKind::Project
                && project.payload.host == original.audience().service
                && execution.payload.kind == ResourceKind::ExecutionService
                && execution.payload.host == facts.audience.service
                && project.payload.host != execution.payload.host
                && wire.invocation.payload.actor == Actor::Service(project.payload.host.clone())
                && wire.invocation.payload.device == original.audience().device
                && wire.invocation.payload.presenter == original.audience().device
                && wire.invocation.issuer == project_device.payload.public_key
                && project_device
                    .payload
                    .uses
                    .contains(&DeviceUse::ServiceInvocation)
                && target.payload.public_key == wire.handoff.target_public_key
                && target.payload.subject == Actor::Service(facts.audience.service.clone()),
            "handoff must connect its live Project host to an independent executor",
        )?;
        require(
            matches!(transport,
                TransportEvidence::VerifiedPeer { origin } if origin == &original.audience().device),
            "handoff requires the actual Project peer origin",
        )?;
        events::verify_signed_event(
            &serde_json::to_string(&wire.handoff.project_dispatch).map_err(invalid_shape)?,
            original.resource().space.as_str(),
            original.resource().resource.as_str(),
            project_device.payload.public_key.as_str(),
        )
        .map_err(invalid_shape)?;
        verify_binding(&binding, project_device.payload.public_key.as_str())
            .map_err(invalid_shape)?;
        require(
            binding.binding.authority_node == event.author,
            "handoff binding is not from its Project authority",
        )?;
        let submission = match admitted {
            Some(admitted) => {
                require(
                    admitted.handoff_hash == hash
                        && admitted.submission.signed_invocation() == &wire.invocation,
                    "handoff admission belongs to a different request",
                )?;
                self.verify_operation_authorization(
                    &admitted.submission,
                    &facts,
                    transport,
                    now_ms,
                )?
            }
            None => self.verify_invocation(&wire.invocation, &facts, transport, now_ms)?,
        };
        let leaf = &self
            .grants
            .get(&wire.invocation.payload.grant_leaf)
            .ok_or(AuthorizationError::InvalidProof(
                "missing execution handoff grant",
            ))?
            .payload;
        require(
            leaf.scope == ResourceScope::Exact(facts.resource.clone())
                && leaf.grantee == Grantee::Actor(wire.invocation.payload.actor.clone())
                && leaf.remaining_delegations == 0
                && leaf.actions.contains(&Action::ExecutionSubmit)
                && leaf.actions.contains(&original.action())
                && leaf.actions.iter().all(|action| {
                    matches!(
                        action,
                        Action::ExecutionSubmit
                            | Action::ExecutionOpen
                            | Action::ExecutionSend
                            | Action::ExecutionSteer
                            | Action::ExecutionInterrupt
                            | Action::ExecutionRespondApproval
                            | Action::OperationStatusRead
                    )
                })
                && leaf.not_before_ms < leaf.expires_at_ms
                && leaf.expires_at_ms < u64::MAX
                && leaf.constraints.context.execution_session == facts.context.execution_session
                && leaf.constraints.context.task == facts.context.task
                && leaf.constraints.context.conversation == facts.context.conversation
                && leaf.constraints.context.executor_kind == facts.context.executor_kind
                && leaf.constraints.context.workspace_resource == facts.context.workspace_resource,
            "execution handoff grant must explicitly bind the action, task, and executor",
        )?;
        let mut dependencies = BTreeMap::new();
        for stamp in original
            .dependencies()
            .iter()
            .chain(submission.dependencies())
        {
            require(
                dependencies
                    .get(&stamp.domain)
                    .is_none_or(|prior| prior == stamp),
                "handoff combines inconsistent authority heads",
            )?;
            dependencies.insert(stamp.domain.clone(), stamp.clone());
        }
        require(
            dependencies.len() <= MAX_POLICY_DOMAINS,
            "handoff has too many policy domains",
        )?;
        let mut usage = source.usage;
        usage.record_authorization(original)?;
        usage.record_authorization(&submission)?;
        usage.record_handoff(wire)?;
        if let Some(admitted) = admitted {
            usage.merge(&admitted.usage);
        }
        usage.enforce_limits()?;
        Ok(VerifiedExecutionHandoff {
            wire: wire.clone(),
            hash,
            original: original.clone(),
            submission,
            original_request_hash: ObjectHash::body(&request)?,
            binding_hash: binding_hash(&binding.binding).map_err(invalid_shape)?,
            binding,
            dependencies: dependencies.into_values().collect(),
            verified_at_ms: now_ms,
            usage,
        })
    }

    pub fn check_execution_handoff_current(
        &self,
        handoff: &VerifiedExecutionHandoff,
        now_ms: u64,
    ) -> Result<()> {
        self.check_current(&handoff.original, now_ms)?;
        self.check_current(&handoff.submission, now_ms)
    }

    pub fn verify_execution_handoff_freshness(
        &self,
        handoff: &VerifiedExecutionHandoff,
        proof: &ExecutionHandoffFreshness,
        now_ms: u64,
    ) -> Result<VerifiedExecutionFreshness> {
        self.check_execution_handoff_current(handoff, now_ms)?;
        let challenge = &proof.challenge;
        let expected = ExecutionHandoffChallenge::bound_to(
            handoff,
            challenge.stage,
            challenge.nonce.clone(),
            challenge.issued_at_ms,
            challenge.expires_at_ms,
        )?;
        require(
            *challenge == expected,
            "execution handoff challenge mismatch",
        )?;
        validate_window(challenge.issued_at_ms, challenge.expires_at_ms, now_ms)?;
        require(
            proof.receipts.len() == handoff.dependencies.len(),
            "handoff requires fresh receipts from both authority closures",
        )?;
        let hash = challenge.hash()?;
        let mut seen = BTreeSet::new();
        let mut budget = Budget::default();
        let mut expires_at_ms = challenge.expires_at_ms.min(handoff.expires_at_ms());
        for signed in &proof.receipts {
            budget.verify(signed)?;
            let receipt = &signed.payload;
            require(
                receipt.version == AUTH_VERSION
                    && receipt.challenge_hash == hash
                    && handoff.dependencies.contains(&receipt.current)
                    && seen.insert(receipt.current.domain.clone()),
                "handoff freshness receipt mismatch",
            )?;
            let authority = match &receipt.current.domain {
                PolicyDomain::Account(id) => self.account(id).map(|document| &document.authority),
                PolicyDomain::Space(id) => self.space(id).map(|document| &document.authority),
            }
            .ok_or(AuthorizationError::InvalidProof(
                "missing handoff freshness authority",
            ))?;
            if self
                .devices
                .values()
                .any(|certificate| certificate.payload.device == authority.device)
            {
                self.find_device(&authority.device)?;
            }
            require(
                signed.issuer == authority.public_key
                    && receipt.issued_at_ms >= challenge.issued_at_ms
                    && receipt.expires_at_ms <= challenge.expires_at_ms,
                "handoff receipt is not from its current authority",
            )?;
            validate_window(receipt.issued_at_ms, receipt.expires_at_ms, now_ms)?;
            expires_at_ms = expires_at_ms.min(receipt.expires_at_ms);
        }
        let mut usage = handoff.usage.clone();
        usage.record_budget(&budget)?;
        usage.record_metadata(challenge)?;
        usage.enforce_limits()?;
        Ok(VerifiedExecutionFreshness {
            handoff_hash: handoff.hash.clone(),
            challenge_hash: hash,
            nonce: challenge.nonce.clone(),
            stage: challenge.stage,
            dependencies: handoff.dependencies.clone(),
            verified_at_ms: now_ms,
            expires_at_ms,
            usage,
        })
    }

    pub fn admit_execution_handoff(
        &self,
        handoff: &VerifiedExecutionHandoff,
        freshness: &VerifiedExecutionFreshness,
        now_ms: u64,
    ) -> Result<VerifiedExecutionAdmission> {
        self.check_execution_handoff_current(handoff, now_ms)?;
        require(
            handoff.submission.phase == AuthorizationPhase::Admission
                && freshness.stage == CheckStage::Admit
                && freshness.handoff_hash == handoff.hash
                && freshness.dependencies == handoff.dependencies
                && freshness.verified_at_ms <= now_ms
                && now_ms < freshness.expires_at_ms,
            "handoff admission freshness mismatch",
        )?;
        Ok(VerifiedExecutionAdmission {
            handoff_hash: handoff.hash.clone(),
            submission: VerifiedAdmission {
                invocation: handoff.submission.invocation.clone(),
                admitted_at_ms: now_ms,
            },
            usage: ExecutionProofUsage::default(),
        })
    }
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionHandoffState {
    Accepted,
    Running,
    Unknown,
    Succeeded,
    Failed,
    Rejected,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ExecutionSessionSnapshot {
    pub original_project: ResourceRef,
    pub execution_resource: ResourceRef,
    pub task: EventRef,
    pub binding_hash: String,
    pub binding_generation: u64,
    pub target: DeviceId,
    pub session_id: String,
    pub revision: u64,
    pub executor: String,
    pub cwd: String,
    pub state: String,
    pub expires_at_ms: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ExecutionHandoffReceipt {
    pub version: u16,
    pub operation_id: String,
    pub original_space_id: SpaceId,
    pub execution_space_id: SpaceId,
    pub handoff_hash: ObjectHash,
    pub target: DeviceId,
    pub session_id: String,
    pub binding_generation: u64,
    pub state: ExecutionHandoffState,
    pub result_hash: ObjectHash,
    pub session: Option<ExecutionSessionSnapshot>,
    pub issued_at_ms: u64,
}
impl sealed::Sealed for ExecutionHandoffReceipt {}
impl SignedObject for ExecutionHandoffReceipt {
    const DOMAIN: &'static str = "execution-handoff-receipt-v2";
}

pub fn verify_execution_handoff_receipt_signature(
    wire: &SignedExecutionHandoff,
    receipt: &Signed<ExecutionHandoffReceipt>,
    result: &Value,
) -> Result<()> {
    receipt.verify_signature()?;
    let (event, proof, _, binding) = original_dispatch(&wire.handoff)?;
    let value = &receipt.payload;
    if let Some(session) = &value.session {
        require(
            session.original_project
                == ResourceRef {
                    space: SpaceId::parse(&event.network_id)?,
                    resource: event.resource_id.clone(),
                }
                && session.execution_resource == wire.handoff.execution_resource
                && session.task.as_str() == binding.binding.task_ref
                && session.binding_hash == binding_hash(&binding.binding).map_err(invalid_shape)?
                && session.binding_generation == binding.binding.generation
                && session.target == wire.handoff.execution_audience.device
                && session.session_id == binding.binding.session_id
                && binding.binding.open_spec["executor"].as_str()
                    == Some(session.executor.as_str())
                && binding.binding.open_spec["cwd"].as_str() == Some(session.cwd.as_str())
                && !session.state.is_empty()
                && session.state.len() <= 128
                && session.expires_at_ms > value.issued_at_ms
                && session.expires_at_ms - value.issued_at_ms <= EXECUTION_LEASE_MS,
            "signed executor session snapshot differs from its task binding",
        )?;
    }
    require(
        receipt.issuer == wire.handoff.target_public_key
            && value.version == AUTH_VERSION
            && value.operation_id == proof.invocation.payload.operation_id
            && value.original_space_id.as_str() == event.network_id
            && value.execution_space_id == wire.handoff.execution_resource.space
            && value.handoff_hash == wire.hash()?
            && value.target == wire.handoff.execution_audience.device
            && value.target == DeviceId::from_key(&receipt.issuer)
            && value.session_id == binding.binding.session_id
            && value.binding_generation == binding.binding.generation
            && value.result_hash == ObjectHash::body(result)?,
        "execution handoff result signature mismatch",
    )
}

impl VerifiedPolicySet {
    pub fn verify_execution_handoff_ack(
        &self,
        wire: &SignedExecutionHandoff,
        original: &VerifiedAuthorization,
        receipt: &Signed<ExecutionHandoffReceipt>,
        result: &Value,
        now_ms: u64,
    ) -> Result<VerifiedExecutionHandoff> {
        verify_execution_handoff_receipt_signature(wire, receipt, result)?;
        require(
            receipt.payload.issued_at_ms >= wire.invocation.payload.not_before_ms
                && receipt.payload.issued_at_ms <= now_ms,
            "execution receipt has an invalid admission clock",
        )?;
        // The executor's signed acknowledgement attests prior acceptance of this
        // exact carrier. Current grants and both live device chains are still checked.
        let mut usage = ExecutionProofUsage::default();
        usage.record_signed(receipt)?;
        let admitted = VerifiedExecutionAdmission {
            handoff_hash: wire.hash()?,
            submission: VerifiedAdmission {
                invocation: wire.invocation.clone(),
                admitted_at_ms: receipt.payload.issued_at_ms,
            },
            usage,
        };
        let project_receipt_origin = TransportEvidence::VerifiedPeer {
            origin: wire.invocation.payload.presenter.clone(),
        };
        self.verify_execution_handoff(
            wire,
            original,
            &project_receipt_origin,
            Some(&admitted),
            now_ms,
        )
    }

    pub fn verify_execution_handoff_receipt(
        &self,
        handoff: &VerifiedExecutionHandoff,
        receipt: &Signed<ExecutionHandoffReceipt>,
        result: &Value,
        now_ms: u64,
    ) -> Result<()> {
        self.check_execution_handoff_current(handoff, now_ms)?;
        verify_execution_handoff_receipt_signature(&handoff.wire, receipt, result)?;
        let value = &receipt.payload;
        require(
            receipt.issuer == handoff.wire.handoff.target_public_key
                && value.version == AUTH_VERSION
                && value.operation_id == handoff.original.operation_id()
                && value.original_space_id == handoff.original.resource().space
                && value.execution_space_id == handoff.wire.handoff.execution_resource.space
                && value.handoff_hash == handoff.hash
                && value.target == handoff.wire.handoff.execution_audience.device
                && value.session_id == handoff.binding.binding.session_id
                && value.binding_generation == handoff.binding.binding.generation
                && value.result_hash == ObjectHash::body(result)?
                && value.issued_at_ms <= now_ms,
            "execution handoff result receipt mismatch",
        )
    }
}

pub fn verify_execution_session_snapshot<'a>(
    receipt: &'a Signed<ExecutionHandoffReceipt>,
    project: &ResourceRef,
    execution: &ResourceRef,
    target_key: &PublicKey,
    binding: &SignedBinding,
    project_key: &PublicKey,
    now_ms: u64,
) -> Result<&'a ExecutionSessionSnapshot> {
    receipt.verify_signature()?;
    verify_binding(binding, project_key.as_str()).map_err(invalid_shape)?;
    let value = &receipt.payload;
    let session = value
        .session
        .as_ref()
        .ok_or(AuthorizationError::InvalidProof(
            "executor receipt has no session snapshot",
        ))?;
    require(
        receipt.issuer == *target_key
            && value.version == AUTH_VERSION
            && value.original_space_id == project.space
            && value.execution_space_id == execution.space
            && value.target == DeviceId::from_key(target_key)
            && value.session_id == binding.binding.session_id
            && value.binding_generation == binding.binding.generation
            && session.original_project == *project
            && session.execution_resource == *execution
            && binding.binding.network_id == project.space.as_str()
            && binding.binding.authority_node
                == node_fingerprint(project_key.as_str()).map_err(invalid_shape)?
            && session.task.as_str() == binding.binding.task_ref
            && session.binding_hash == binding_hash(&binding.binding).map_err(invalid_shape)?
            && session.binding_generation == binding.binding.generation
            && session.target == value.target
            && binding.binding.target_node
                == node_fingerprint(target_key.as_str()).map_err(invalid_shape)?
            && session.session_id == binding.binding.session_id
            && binding.binding.open_spec["executor"].as_str() == Some(session.executor.as_str())
            && binding.binding.open_spec["cwd"].as_str() == Some(session.cwd.as_str())
            && !session.state.is_empty()
            && session.state.len() <= 128
            && value.issued_at_ms <= now_ms
            && now_ms < session.expires_at_ms
            && session.expires_at_ms - value.issued_at_ms <= EXECUTION_LEASE_MS,
        "executor session snapshot differs from the pinned scope, key, binding or clock",
    )?;
    Ok(session)
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionResolutionPurpose {
    ResolveUnknown,
    ReconcileTerminal,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ExecutionHandoffResolution {
    pub version: u16,
    pub purpose: ExecutionResolutionPurpose,
    pub observation: ExecutionHandoffResult,
    pub account_admission: AdmissionProof,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct SignedExecutionHandoffResolution {
    pub resolution: ExecutionHandoffResolution,
    pub invocation: Signed<Invocation>,
}

impl SignedExecutionHandoffResolution {
    pub fn hash(&self) -> Result<ObjectHash> {
        require(
            canonical(self)?.len() <= MAX_EXECUTION_HANDOFF_BYTES,
            "execution resolution is too large",
        )?;
        ObjectHash::body(self)
    }
}

pub fn execution_resolution_account_facts(
    value: &ExecutionHandoffResolution,
) -> Result<RequestFacts> {
    require(
        value.version == AUTH_VERSION,
        "invalid execution resolution version",
    )?;
    let unknown = &value.observation;
    verify_execution_handoff_receipt_signature(
        &unknown.handoff,
        &unknown.receipt,
        &unknown.result,
    )?;
    let (event, original, request, binding) = original_dispatch(&unknown.handoff.handoff)?;
    let proof = &value.account_admission;
    let body = &proof.source_body;
    let fields = body.as_object().ok_or(AuthorizationError::InvalidProof(
        "invalid resolution source",
    ))?;
    require(
        fields.keys().all(|key| {
            matches!(
                key.as_str(),
                "method"
                    | "space_id"
                    | "resource_id"
                    | "authorization_id"
                    | "operation_id"
                    | "conversation_id"
                    | "task_id"
                    | "task_ref"
                    | "target_node"
                    | "session_id"
                    | "outcome"
                    | "evidence"
                    | "execution_resource"
                    | "target_public_key"
                    | "execution_grant"
                    | "original_handoff_hash"
                    | "original_body_hash"
                    | "unknown_receipt_hash"
                    | "resolution_purpose"
                    | "execution_observation"
            )
        }),
        "resolution cannot disclose unrelated fields",
    )?;
    let outcome = body["outcome"].as_str().unwrap_or_default();
    let evidence = body["evidence"].as_str().unwrap_or_default();
    require(
        (match value.purpose {
            ExecutionResolutionPurpose::ResolveUnknown => {
                unknown.receipt.payload.state == ExecutionHandoffState::Unknown
            }
            ExecutionResolutionPurpose::ReconcileTerminal => {
                matches!(
                    unknown.receipt.payload.state,
                    ExecutionHandoffState::Succeeded
                        | ExecutionHandoffState::Failed
                        | ExecutionHandoffState::Rejected
                ) && serde_json::to_value(unknown.receipt.payload.state).map_err(invalid_shape)?
                    == body["outcome"]
            }
        }) && body["resolution_purpose"]
            == serde_json::to_value(value.purpose).map_err(invalid_shape)?
            && body["execution_observation"]
                == serde_json::to_value(&value.observation).map_err(invalid_shape)?
            && unknown.receipt.payload.issued_at_ms <= proof.admitted_at_ms
            && matches!(proof.invocation.payload.actor, Actor::Account(_))
            && proof.invocation.payload.context.sdk_session.is_none()
            && body["method"] == "resolve_unknown"
            && body["space_id"].as_str() == Some(event.network_id.as_str())
            && body["resource_id"].as_str() == Some(event.resource_id.as_str())
            && body["authorization_id"].as_str()
                == Some(proof.invocation.payload.operation_id.as_str())
            && proof.invocation.payload.operation_id != original.invocation.payload.operation_id
            && body["operation_id"].as_str()
                == Some(original.invocation.payload.operation_id.as_str())
            && body["conversation_id"].as_str() == Some(event.scope_id.as_str())
            && body["task_id"] == request["task_id"]
            && body["task_ref"].as_str() == Some(binding.binding.task_ref.as_str())
            && body["target_node"].as_str() == Some(binding.binding.target_node.as_str())
            && body["session_id"].as_str() == Some(binding.binding.session_id.as_str())
            && body["execution_resource"]
                == serde_json::to_value(&unknown.handoff.handoff.execution_resource)
                    .map_err(invalid_shape)?
            && body["target_public_key"]
                == serde_json::to_value(&unknown.handoff.handoff.target_public_key)
                    .map_err(invalid_shape)?
            && body["original_handoff_hash"].as_str() == Some(unknown.handoff.hash()?.as_str())
            && body["original_body_hash"].as_str()
                == Some(original.invocation.payload.body_hash.as_str())
            && body["unknown_receipt_hash"].as_str() == Some(unknown.receipt.hash()?.as_str())
            && matches!(outcome, "succeeded" | "failed" | "rejected")
            && evidence.trim().len() >= 12
            && evidence.len() <= 65_536,
        "resolution differs from the accepted operation, unknown receipt or Account intent",
    )?;
    ObjectHash::parse(body["execution_grant"].as_str().unwrap_or_default())?;
    let context = InvocationContext {
        conversation: Some(event.scope_id),
        task: Some(EventRef::parse(&binding.binding.task_ref)?),
        sdk_session: None,
        execution_session: Some(SessionRef {
            target: unknown.handoff.handoff.execution_audience.device.clone(),
            session_id: binding.binding.session_id,
        }),
        executor_kind: binding.binding.open_spec["executor"]
            .as_str()
            .map(str::to_owned),
        workspace_resource: Some(event.resource_id.clone()),
    };
    Ok(RequestFacts {
        resource: ResourceRef {
            space: SpaceId::parse(&event.network_id)?,
            resource: event.resource_id,
        },
        action: Action::OperationResolve,
        audience: proof.invocation.payload.audience.clone(),
        operation_id: proof.invocation.payload.operation_id.clone(),
        body_hash: ObjectHash::body(body)?,
        context,
    })
}

pub fn execution_resolution_facts(
    value: &ExecutionHandoffResolution,
    resolver: &VerifiedAuthorization,
) -> Result<RequestFacts> {
    let account = execution_resolution_account_facts(value)?;
    require(
        resolver.signed_invocation() == &value.account_admission.invocation
            && resolver.resource() == &account.resource
            && resolver.action() == Action::OperationResolve
            && resolver.body_hash() == &account.body_hash
            && resolver.signed_invocation().payload.context == account.context,
        "execution resolution has another Account authorization",
    )?;
    Ok(RequestFacts {
        resource: value.observation.handoff.handoff.execution_resource.clone(),
        action: Action::ExecutionResolve,
        audience: value.observation.handoff.handoff.execution_audience.clone(),
        operation_id: account.operation_id,
        body_hash: ObjectHash::body(value)?,
        context: InvocationContext {
            workspace_resource: Some(
                value
                    .observation
                    .handoff
                    .handoff
                    .execution_resource
                    .resource
                    .clone(),
            ),
            ..account.context
        },
    })
}

pub fn execution_resolution_result(value: &ExecutionHandoffResolution) -> Result<Value> {
    execution_resolution_account_facts(value)?;
    let (_, original, _, _) = original_dispatch(&value.observation.handoff.handoff)?;
    Ok(serde_json::json!({
        "resolver": value.account_admission.invocation.payload.actor,
        "invocation_hash": value.account_admission.invocation.hash()?,
        "evidence": value.account_admission.source_body["evidence"],
        "resolution_id": value.account_admission.invocation.payload.operation_id,
        "original_actor": original.invocation.payload.actor,
        "original_invocation_hash": original.invocation.hash()?,
        "unknown_receipt_hash": value.observation.receipt.hash()?,
    }))
}

#[derive(Clone, Debug)]
pub struct VerifiedExecutionResolution {
    wire: SignedExecutionHandoffResolution,
    hash: ObjectHash,
    resolver: VerifiedAuthorization,
    submission: VerifiedAuthorization,
    dependencies: Vec<PolicyStamp>,
    verified_at_ms: u64,
    usage: ExecutionProofUsage,
}
impl VerifiedExecutionResolution {
    pub fn with_admission_checkpoint(
        mut self,
        receipt: &Signed<ExecutionHandoffAdmissionReceipt>,
        admission: &ExecutionHandoffFreshness,
        transport: &StoredTransport,
        admitted_at_ms: u64,
    ) -> Result<Self> {
        let checkpoint = verify_execution_handoff_admission_checkpoint(
            &self.wire.resolution.observation.handoff,
            receipt,
            admission,
            transport,
            admitted_at_ms,
        )?;
        self.usage.merge(&checkpoint.usage);
        self.usage.enforce_limits()?;
        Ok(self)
    }
    pub fn proof_usage(&self) -> &ExecutionProofUsage {
        &self.usage
    }
    pub fn wire(&self) -> &SignedExecutionHandoffResolution {
        &self.wire
    }
    pub fn hash(&self) -> &ObjectHash {
        &self.hash
    }
    pub fn resolver(&self) -> &VerifiedAuthorization {
        &self.resolver
    }
    pub fn submission(&self) -> &VerifiedAuthorization {
        &self.submission
    }
    pub fn dependencies(&self) -> &[PolicyStamp] {
        &self.dependencies
    }
    pub fn expires_at_ms(&self) -> u64 {
        self.resolver
            .expires_at_ms()
            .min(self.submission.expires_at_ms())
    }
}

#[derive(Clone, Debug)]
pub struct VerifiedExecutionResolutionAdmission {
    resolution_hash: ObjectHash,
    submission: VerifiedAdmission,
    usage: ExecutionProofUsage,
}

pub fn verify_execution_resolution_admission_checkpoint(
    result: &ExecutionHandoffResolutionResult,
    freshness: &ExecutionHandoffResolutionFreshness,
    transport: &StoredTransport,
    resolved_at_ms: u64,
) -> Result<VerifiedExecutionResolutionAdmission> {
    verify_execution_resolution_receipt_signature(result)?;
    let invocation = &result.resolution.invocation.payload;
    require(
        result.freshness == *freshness
            && result.receipt.payload.issued_at_ms == resolved_at_ms
            && invocation.not_before_ms <= resolved_at_ms
            && resolved_at_ms < invocation.expires_at_ms
            && transport.clone().into_evidence()
                == TransportEvidence::VerifiedPeer {
                    origin: invocation.device.clone(),
                },
        "executor resolution checkpoint changed",
    )?;
    let mut usage = ExecutionProofUsage::default();
    usage.record_signed(&result.receipt)?;
    usage.record_signed(&result.handoff_receipt)?;
    usage.record_metadata(freshness)?;
    usage.record_metadata(transport)?;
    Ok(VerifiedExecutionResolutionAdmission {
        resolution_hash: result.resolution.hash()?,
        submission: VerifiedAdmission {
            invocation: result.resolution.invocation.clone(),
            admitted_at_ms: resolved_at_ms,
        },
        usage,
    })
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ExecutionHandoffResolutionChallenge {
    pub version: u16,
    pub resolution_hash: ObjectHash,
    pub resolver_invocation_hash: ObjectHash,
    pub submission_invocation_hash: ObjectHash,
    pub policies: Vec<PolicyStamp>,
    pub nonce: Nonce,
    pub issued_at_ms: u64,
    pub expires_at_ms: u64,
}
impl sealed::Sealed for ExecutionHandoffResolutionChallenge {}
impl SignedObject for ExecutionHandoffResolutionChallenge {
    const DOMAIN: &'static str = "execution-handoff-resolution-challenge-v2";
}
impl ExecutionHandoffResolutionChallenge {
    pub fn new(
        value: &VerifiedExecutionResolution,
        nonce: Nonce,
        issued_at_ms: u64,
        expires_at_ms: u64,
    ) -> Result<Self> {
        require(
            issued_at_ms >= value.verified_at_ms,
            "resolution challenge clock moved backwards",
        )?;
        Self::bound_to(value, nonce, issued_at_ms, expires_at_ms)
    }

    fn bound_to(
        value: &VerifiedExecutionResolution,
        nonce: Nonce,
        issued_at_ms: u64,
        expires_at_ms: u64,
    ) -> Result<Self> {
        validate_window(issued_at_ms, expires_at_ms, issued_at_ms)?;
        require(
            expires_at_ms - issued_at_ms <= EXECUTION_LEASE_MS,
            "resolution challenge is too long",
        )?;
        Ok(Self {
            version: AUTH_VERSION,
            resolution_hash: value.hash.clone(),
            resolver_invocation_hash: value.resolver.invocation_hash().clone(),
            submission_invocation_hash: value.submission.invocation_hash().clone(),
            policies: value.dependencies.clone(),
            nonce,
            issued_at_ms,
            expires_at_ms,
        })
    }
    pub fn hash(&self) -> Result<ObjectHash> {
        Ok(ObjectHash(format!(
            "h2_{}",
            digest(Self::DOMAIN, &canonical(self)?)
        )))
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ExecutionHandoffResolutionFreshness {
    pub challenge: ExecutionHandoffResolutionChallenge,
    pub receipts: Vec<Signed<FreshnessReceipt>>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ExecutionHandoffResolutionReceipt {
    pub version: u16,
    pub resolution_hash: ObjectHash,
    pub freshness_hash: ObjectHash,
    pub handoff_receipt_hash: ObjectHash,
    pub issued_at_ms: u64,
}
impl sealed::Sealed for ExecutionHandoffResolutionReceipt {}
impl SignedObject for ExecutionHandoffResolutionReceipt {
    const DOMAIN: &'static str = "execution-handoff-resolution-receipt-v2";
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ExecutionHandoffResolutionResult {
    pub resolution: SignedExecutionHandoffResolution,
    pub freshness: ExecutionHandoffResolutionFreshness,
    pub handoff_receipt: Signed<ExecutionHandoffReceipt>,
    pub result: Value,
    pub receipt: Signed<ExecutionHandoffResolutionReceipt>,
}

pub fn verify_execution_resolution_receipt_signature(
    value: &ExecutionHandoffResolutionResult,
) -> Result<()> {
    value.receipt.verify_signature()?;
    let resolution = &value.resolution;
    verify_execution_handoff_receipt_signature(
        &resolution.resolution.observation.handoff,
        &value.handoff_receipt,
        &value.result,
    )?;
    require(
        resolution.resolution.purpose == ExecutionResolutionPurpose::ResolveUnknown
            && value.receipt.issuer
                == resolution
                    .resolution
                    .observation
                    .handoff
                    .handoff
                    .target_public_key
            && value.receipt.payload.version == AUTH_VERSION
            && value.receipt.payload.resolution_hash == resolution.hash()?
            && value.receipt.payload.freshness_hash == ObjectHash::body(&value.freshness)?
            && value.freshness.challenge.resolution_hash == resolution.hash()?
            && value.freshness.challenge.issued_at_ms <= value.receipt.payload.issued_at_ms
            && value.receipt.payload.issued_at_ms < value.freshness.challenge.expires_at_ms
            && value.receipt.payload.handoff_receipt_hash == value.handoff_receipt.hash()?
            && value.receipt.payload.issued_at_ms == value.handoff_receipt.payload.issued_at_ms
            && value.receipt.payload.issued_at_ms
                >= resolution.resolution.account_admission.admitted_at_ms
            && serde_json::to_value(value.handoff_receipt.payload.state).map_err(invalid_shape)?
                == resolution.resolution.account_admission.source_body["outcome"]
            && value.result == execution_resolution_result(&resolution.resolution)?,
        "execution resolution receipt differs from the explicit Account decision",
    )
}

impl VerifiedPolicySet {
    pub fn verify_execution_resolution_account_checkpoint(
        &self,
        wire: &SignedExecutionHandoffResolution,
        transport: &TransportEvidence,
        now_ms: u64,
    ) -> Result<VerifiedAuthorization> {
        let proof = &wire.resolution.account_admission;
        let source = &proof.invocation.payload;
        let project = self
            .resource(&source.resource.space, &source.resource.resource)
            .ok_or(AuthorizationError::InvalidProof(
                "missing resolution source Project",
            ))?;
        let certificate = self.find_device(&source.audience.device)?;
        require(
            project.payload.kind == ResourceKind::Project
                && project.payload.host == source.audience.service
                && certificate.payload.subject == Actor::Service(project.payload.host.clone())
                && certificate
                    .payload
                    .uses
                    .contains(&DeviceUse::ServiceInvocation)
                && wire.invocation.issuer == certificate.payload.public_key
                && wire.invocation.payload.actor == certificate.payload.subject
                && wire.invocation.payload.device == certificate.payload.device
                && wire.invocation.payload.presenter == certificate.payload.device
                && wire.invocation.payload.action == Action::ExecutionResolve
                && wire.invocation.payload.body_hash == ObjectHash::body(&wire.resolution)?
                && matches!(transport, TransportEvidence::VerifiedPeer { origin } if origin == &certificate.payload.device),
            "resolution admission checkpoint is not from its actual current Project host",
        )?;
        let mut budget = Budget::default();
        self.validate_certificate(
            certificate,
            now_ms,
            &mut budget,
            &mut BTreeMap::new(),
            &mut BTreeSet::new(),
        )?;
        budget.verify(project)?;
        budget.verify(&wire.invocation)?;
        let facts = execution_resolution_account_facts(&wire.resolution)?;
        let admitted = attested_admission(
            &proof.invocation,
            &proof.challenge,
            proof.admitted_at_ms,
            now_ms,
        )?;
        self.verify_operation_authorization(
            &admitted,
            &facts,
            &proof.transport.clone().into_evidence(),
            now_ms,
        )
    }

    pub fn verify_execution_resolution(
        &self,
        wire: &SignedExecutionHandoffResolution,
        resolver: &VerifiedAuthorization,
        transport: &TransportEvidence,
        admitted: Option<&VerifiedExecutionResolutionAdmission>,
        now_ms: u64,
    ) -> Result<VerifiedExecutionResolution> {
        self.check_current(resolver, now_ms)?;
        let hash = wire.hash()?;
        let facts = execution_resolution_facts(&wire.resolution, resolver)?;
        require(
            wire.resolution.account_admission.admitted_at_ms <= now_ms,
            "Account resolution admission is from the future",
        )?;
        let project = self
            .resource(&resolver.resource().space, &resolver.resource().resource)
            .ok_or(AuthorizationError::InvalidProof(
                "missing resolution Project",
            ))?;
        let execution = self
            .resource(&facts.resource.space, &facts.resource.resource)
            .ok_or(AuthorizationError::InvalidProof(
                "missing resolution ExecutionService",
            ))?;
        let project_device = self.find_device(&resolver.audience().device)?;
        let target = self.find_device(&facts.audience.device)?;
        require(
            project.payload.kind == ResourceKind::Project
                && project.payload.host == resolver.audience().service
                && execution.payload.kind == ResourceKind::ExecutionService
                && execution.payload.host == facts.audience.service
                && project.payload.host != execution.payload.host
                && wire.invocation.payload.actor == Actor::Service(project.payload.host.clone())
                && wire.invocation.payload.device == resolver.audience().device
                && wire.invocation.payload.presenter == resolver.audience().device
                && wire.invocation.issuer == project_device.payload.public_key
                && project_device
                    .payload
                    .uses
                    .contains(&DeviceUse::ServiceInvocation)
                && target.payload.public_key
                    == wire
                        .resolution
                        .observation
                        .handoff
                        .handoff
                        .target_public_key
                && target.payload.subject == Actor::Service(facts.audience.service.clone())
                && matches!(transport, TransportEvidence::VerifiedPeer{origin} if origin == &resolver.audience().device),
            "resolution requires the current Project host and actual peer origin",
        )?;
        let submission = if let Some(admitted) = admitted {
            require(
                admitted.resolution_hash == hash
                    && admitted.submission.signed_invocation() == &wire.invocation,
                "resolution admission belongs to another request",
            )?;
            self.verify_operation_authorization(&admitted.submission, &facts, transport, now_ms)?
        } else {
            self.verify_invocation(&wire.invocation, &facts, transport, now_ms)?
        };
        let leaf = &self
            .grants
            .get(&wire.invocation.payload.grant_leaf)
            .ok_or(AuthorizationError::InvalidProof(
                "missing explicit executor resolution grant",
            ))?
            .payload;
        require(
            wire.resolution.account_admission.source_body["execution_grant"].as_str()
                == Some(wire.invocation.payload.grant_leaf.as_str())
                && leaf.scope == ResourceScope::Exact(facts.resource.clone())
                && leaf.grantee == Grantee::Actor(wire.invocation.payload.actor.clone())
                && leaf.remaining_delegations == 0
                && leaf.actions.len() == 2
                && leaf.actions.contains(&Action::ExecutionResolve)
                && leaf.actions.contains(&Action::OperationResolve)
                && leaf.not_before_ms < leaf.expires_at_ms
                && leaf.expires_at_ms < u64::MAX
                && leaf.constraints.context == facts.context,
            "executor resolution grant must bind only explicit resolution and this task/session",
        )?;
        let mut dependencies = BTreeMap::new();
        for stamp in resolver
            .dependencies()
            .iter()
            .chain(submission.dependencies())
        {
            require(
                dependencies
                    .get(&stamp.domain)
                    .is_none_or(|prior| prior == stamp),
                "resolution combines inconsistent policy heads",
            )?;
            dependencies.insert(stamp.domain.clone(), stamp.clone());
        }
        require(
            dependencies.len() <= MAX_POLICY_DOMAINS,
            "resolution has too many policy domains",
        )?;
        let mut usage = ExecutionProofUsage::default();
        usage.record_authorization(resolver)?;
        usage.record_authorization(&submission)?;
        usage.record_admission_kind(&wire.resolution.account_admission, true)?;
        usage.record_signed(&wire.resolution.observation.receipt)?;
        usage.record_signed(&wire.invocation)?;
        usage.record_metadata(
            &serde_json::json!({"version":wire.resolution.version,"purpose":wire.resolution.purpose,
            "result_hash":ObjectHash::body(&wire.resolution.observation.result)?}),
        )?;
        if let Some(admitted) = admitted {
            usage.merge(&admitted.usage);
        }
        usage.enforce_limits()?;
        Ok(VerifiedExecutionResolution {
            wire: wire.clone(),
            hash,
            resolver: resolver.clone(),
            submission,
            dependencies: dependencies.into_values().collect(),
            verified_at_ms: now_ms,
            usage,
        })
    }

    pub fn verify_execution_resolution_freshness(
        &self,
        value: &VerifiedExecutionResolution,
        proof: &ExecutionHandoffResolutionFreshness,
        now_ms: u64,
    ) -> Result<u64> {
        self.check_current(&value.resolver, now_ms)?;
        self.check_current(&value.submission, now_ms)?;
        let challenge = &proof.challenge;
        require(
            *challenge
                == ExecutionHandoffResolutionChallenge::bound_to(
                    value,
                    challenge.nonce.clone(),
                    challenge.issued_at_ms,
                    challenge.expires_at_ms,
                )?,
            "execution resolution challenge mismatch",
        )?;
        validate_window(challenge.issued_at_ms, challenge.expires_at_ms, now_ms)?;
        require(
            proof.receipts.len() == value.dependencies.len(),
            "resolution requires both current authority closures",
        )?;
        let hash = challenge.hash()?;
        let mut seen = BTreeSet::new();
        let mut budget = Budget::default();
        let mut expiry = challenge.expires_at_ms.min(value.expires_at_ms());
        for signed in &proof.receipts {
            budget.verify(signed)?;
            let receipt = &signed.payload;
            require(
                receipt.version == AUTH_VERSION
                    && receipt.challenge_hash == hash
                    && value.dependencies.contains(&receipt.current)
                    && seen.insert(receipt.current.domain.clone()),
                "resolution freshness receipt mismatch",
            )?;
            let authority = match &receipt.current.domain {
                PolicyDomain::Account(id) => self.account(id).map(|d| &d.authority),
                PolicyDomain::Space(id) => self.space(id).map(|d| &d.authority),
            }
            .ok_or(AuthorizationError::InvalidProof(
                "missing resolution authority",
            ))?;
            if self
                .devices
                .values()
                .any(|cert| cert.payload.device == authority.device)
            {
                self.find_device(&authority.device)?;
            }
            require(
                signed.issuer == authority.public_key
                    && receipt.issued_at_ms >= challenge.issued_at_ms
                    && receipt.expires_at_ms <= challenge.expires_at_ms,
                "resolution receipt is not from its current authority",
            )?;
            validate_window(receipt.issued_at_ms, receipt.expires_at_ms, now_ms)?;
            expiry = expiry.min(receipt.expires_at_ms);
        }
        let mut usage = value.usage.clone();
        usage.record_budget(&budget)?;
        usage.record_metadata(challenge)?;
        usage.enforce_limits()?;
        Ok(expiry)
    }

    pub fn admit_execution_resolution(
        &self,
        value: &VerifiedExecutionResolution,
        proof: &ExecutionHandoffResolutionFreshness,
        now_ms: u64,
    ) -> Result<VerifiedExecutionResolutionAdmission> {
        self.verify_execution_resolution_freshness(value, proof, now_ms)?;
        require(
            value.submission.phase == AuthorizationPhase::Admission,
            "resolution was already admitted",
        )?;
        Ok(VerifiedExecutionResolutionAdmission {
            resolution_hash: value.hash.clone(),
            submission: VerifiedAdmission {
                invocation: value.wire.invocation.clone(),
                admitted_at_ms: now_ms,
            },
            usage: ExecutionProofUsage::default(),
        })
    }

    pub fn verify_execution_resolution_ack(
        &self,
        result: &ExecutionHandoffResolutionResult,
        resolver: &VerifiedAuthorization,
        now_ms: u64,
    ) -> Result<VerifiedExecutionResolution> {
        verify_execution_resolution_receipt_signature(result)?;
        require(
            result.receipt.payload.issued_at_ms <= now_ms
                && result.receipt.payload.issued_at_ms
                    >= result.resolution.invocation.payload.not_before_ms,
            "resolution acknowledgement has an invalid clock",
        )?;
        let mut usage = ExecutionProofUsage::default();
        usage.record_signed(&result.receipt)?;
        usage.record_signed(&result.handoff_receipt)?;
        let admission = VerifiedExecutionResolutionAdmission {
            resolution_hash: result.resolution.hash()?,
            submission: VerifiedAdmission {
                invocation: result.resolution.invocation.clone(),
                admitted_at_ms: result.receipt.payload.issued_at_ms,
            },
            usage,
        };
        self.verify_execution_resolution(
            &result.resolution,
            resolver,
            &TransportEvidence::VerifiedPeer {
                origin: result.resolution.invocation.payload.presenter.clone(),
            },
            Some(&admission),
            now_ms,
        )
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ExecutionHandoffTerminalReconciliationResult {
    pub reconciliation: SignedExecutionHandoffResolution,
    pub freshness: ExecutionHandoffResolutionFreshness,
}

pub struct VerifiedExecutionTerminalReconciliation {
    authorization: VerifiedExecutionResolution,
    freshness_hash: ObjectHash,
}
impl VerifiedExecutionTerminalReconciliation {
    pub fn proof_usage(&self) -> &ExecutionProofUsage {
        self.authorization.proof_usage()
    }
    pub fn freshness_hash(&self) -> &ObjectHash {
        &self.freshness_hash
    }
    pub fn resolver(&self) -> &VerifiedAuthorization {
        self.authorization.resolver()
    }
    pub fn submission(&self) -> &VerifiedAuthorization {
        self.authorization.submission()
    }
    pub fn dependencies(&self) -> &[PolicyStamp] {
        self.authorization.dependencies()
    }
    pub fn wire(&self) -> &SignedExecutionHandoffResolution {
        self.authorization.wire()
    }
}

impl VerifiedPolicySet {
    pub fn verify_execution_terminal_reconciliation(
        &self,
        result: &ExecutionHandoffTerminalReconciliationResult,
        resolver: &VerifiedAuthorization,
        now_ms: u64,
    ) -> Result<VerifiedExecutionTerminalReconciliation> {
        require(
            result.reconciliation.resolution.purpose
                == ExecutionResolutionPurpose::ReconcileTerminal,
            "terminal reconciliation requires its independent purpose",
        )?;
        let authorization = self.verify_execution_resolution(
            &result.reconciliation,
            resolver,
            &TransportEvidence::VerifiedPeer {
                origin: result.reconciliation.invocation.payload.presenter.clone(),
            },
            None,
            now_ms,
        )?;
        self.verify_execution_resolution_freshness(&authorization, &result.freshness, now_ms)?;
        Ok(VerifiedExecutionTerminalReconciliation {
            authorization,
            freshness_hash: ObjectHash::body(&result.freshness)?,
        })
    }
}

#[derive(Clone, Debug, Default)]
pub struct ExecutionProofUsage {
    objects: BTreeMap<ObjectHash, usize>,
    metadata: BTreeMap<ObjectHash, usize>,
    signatures: BTreeSet<ObjectHash>,
    max_chain_depth: usize,
    historical_wrappers: BTreeMap<ObjectHash, usize>,
    task_payloads: BTreeMap<ObjectHash, usize>,
    actual: Option<crate::signature_verification::SignatureVerificationCounts>,
}
#[derive(Clone, Debug, Serialize)]
pub struct ExecutionProofCounts {
    pub authorization_bytes: usize,
    pub unique_signed_objects: usize,
    pub unique_signatures: usize,
    pub max_chain_depth: usize,
    pub historical_wrappers: usize,
    pub historical_wrapper_bytes: usize,
    pub task_payload_bytes: usize,
    pub actual: Option<crate::signature_verification::SignatureVerificationCounts>,
}
impl ExecutionProofUsage {
    pub fn record_chain_depth(&mut self, depth: usize) {
        self.max_chain_depth = self.max_chain_depth.max(depth);
    }
    pub fn record_actual_signature_counts(
        &mut self,
        counts: crate::signature_verification::SignatureVerificationCounts,
    ) {
        self.actual = Some(counts);
    }
    pub fn merge(&mut self, other: &Self) {
        self.objects.extend(
            other
                .objects
                .iter()
                .map(|(hash, size)| (hash.clone(), *size)),
        );
        self.metadata.extend(
            other
                .metadata
                .iter()
                .map(|(hash, size)| (hash.clone(), *size)),
        );
        self.signatures.extend(other.signatures.iter().cloned());
        self.max_chain_depth = self.max_chain_depth.max(other.max_chain_depth);
        self.historical_wrappers.extend(
            other
                .historical_wrappers
                .iter()
                .map(|(hash, size)| (hash.clone(), *size)),
        );
        self.task_payloads.extend(
            other
                .task_payloads
                .iter()
                .map(|(hash, size)| (hash.clone(), *size)),
        );
    }
    pub fn record_authorization(&mut self, auth: &VerifiedAuthorization) -> Result<()> {
        self.record_budget(&auth.budget)?;
        self.max_chain_depth = self.max_chain_depth.max(auth.grant_path().len());
        Ok(())
    }
    pub(super) fn record_budget(&mut self, budget: &Budget) -> Result<()> {
        for (hash, size) in &budget.object_bytes {
            self.objects.insert(hash.clone(), *size);
        }
        for signature in &budget.signatures {
            self.signatures.insert(ObjectHash::body(signature)?);
        }
        self.signatures
            .extend(budget.external_signatures.iter().cloned());
        Ok(())
    }
    pub fn record_signed<T: SignedObject>(&mut self, value: &Signed<T>) -> Result<()> {
        self.objects.insert(value.hash()?, canonical(value)?.len());
        let message_hash = value.hash()?;
        self.signatures.insert(ObjectHash::body(&(
            &value.issuer,
            T::DOMAIN,
            message_hash,
            &value.signature,
        ))?);
        Ok(())
    }
    pub fn record_metadata<T: Serialize>(&mut self, value: &T) -> Result<()> {
        self.metadata
            .insert(ObjectHash::body(value)?, canonical(value)?.len());
        Ok(())
    }
    fn record_historical_wrapper<T: SignedObject>(&mut self, value: &Signed<T>) -> Result<()> {
        let hash = value.hash()?;
        let size = canonical(value)?.len();
        self.historical_wrappers.insert(hash.clone(), size);
        self.metadata.insert(hash, size);
        Ok(())
    }
    pub fn record_admission(&mut self, proof: &AdmissionProof) -> Result<()> {
        self.record_admission_kind(proof, false)
    }
    fn record_admission_kind(&mut self, proof: &AdmissionProof, checkpointed: bool) -> Result<()> {
        require(
            proof.receipts.len() <= MAX_POLICY_DOMAINS
                && proof.challenge.policies.len() <= MAX_POLICY_DOMAINS,
            "too many historical admission items",
        )?;
        let mut source = proof.source_body.clone();
        if source["method"] == "create_task" {
            for field in ["title", "goal", "acceptance"] {
                let value = source[field]
                    .as_str()
                    .ok_or(AuthorizationError::InvalidProof(
                        "invalid typed task payload",
                    ))?;
                self.task_payloads
                    .insert(ObjectHash::body(&value)?, value.len());
                source[field] = serde_json::json!({"payload_hash":ObjectHash::body(&value)?,"utf8_bytes":value.len()});
            }
        }
        if source["method"] == "operate" {
            if matches!(
                source["request"]["action"]["type"].as_str(),
                Some("send" | "steer")
            ) {
                let text = source["request"]["action"]["text"].as_str().ok_or(
                    AuthorizationError::InvalidProof("invalid typed execution text"),
                )?;
                source["request"]["action"]["text"] = serde_json::json!({"payload_hash":ObjectHash::body(&text)?,"utf8_bytes":text.len()});
            }
            let binding: SignedBinding =
                serde_json::from_value(source["request"]["binding"].clone())
                    .map_err(invalid_shape)?;
            self.record_binding(&binding)?;
            source["request"]["binding"] =
                serde_json::json!({"signed_binding_hash":ObjectHash::body(&binding)?});
            if !source["execution_session_receipt"].is_null() {
                let receipt: Signed<ExecutionHandoffReceipt> =
                    serde_json::from_value(source["execution_session_receipt"].clone())
                        .map_err(invalid_shape)?;
                self.record_signed(&receipt)?;
                source["execution_session_receipt"] =
                    serde_json::json!({"signed_receipt_hash":receipt.hash()?});
            }
        }
        if source["method"] == "resolve_unknown" && !source["execution_observation"].is_null() {
            let observation: ExecutionHandoffResult =
                serde_json::from_value(source["execution_observation"].clone())
                    .map_err(invalid_shape)?;
            self.record_historical_handoff(&observation.handoff)?;
            self.record_signed(&observation.receipt)?;
            source["execution_observation"] =
                serde_json::json!({"observation_hash":ObjectHash::body(&observation)?});
        }
        if checkpointed {
            self.record_historical_wrapper(&proof.invocation)?;
            for receipt in &proof.receipts {
                self.record_historical_wrapper(receipt)?;
            }
        } else {
            self.record_signed(&proof.invocation)?;
            for receipt in &proof.receipts {
                self.record_signed(receipt)?;
            }
        }
        self.record_metadata(&serde_json::json!({"source_body_hash":ObjectHash::body(&proof.source_body)?,"source_metadata":source,
            "challenge":proof.challenge,"admitted_at_ms":proof.admitted_at_ms,"transport":proof.transport}))
    }
    pub fn record_binding(&mut self, binding: &SignedBinding) -> Result<()> {
        let hash = ObjectHash::body(binding)?;
        self.objects.insert(hash.clone(), canonical(binding)?.len());
        self.signatures.insert(ObjectHash::body(&(
            BINDING_DOMAIN,
            &binding.binding.authority_node,
            binding_hash(&binding.binding).map_err(invalid_shape)?,
            &binding.signature,
        ))?);
        if let Some(seal) = &binding.binding.previous_seal {
            self.objects
                .insert(ObjectHash::body(seal)?, canonical(seal)?.len());
            self.signatures.insert(ObjectHash::body(&(
                SEAL_DOMAIN,
                &seal.seal.target_node,
                ObjectHash::body(&seal.seal)?,
                &seal.signature,
            ))?);
        }
        Ok(())
    }
    pub fn record_event(&mut self, envelope: &SignedEvent) -> Result<()> {
        let event: Event = serde_json::from_str(&envelope.canonical_body).map_err(invalid_shape)?;
        let EventCause::Invocation { admission } = &event.cause else {
            return Err(AuthorizationError::InvalidProof(
                "capsule event has no original admission",
            ));
        };
        self.record_admission_kind(admission, true)?;
        self.record_event_signature(envelope, &event, admission)
    }
    fn record_event_signature(
        &mut self,
        envelope: &SignedEvent,
        event: &Event,
        admission: &AdmissionProof,
    ) -> Result<()> {
        let metadata = serde_json::json!({"hash":envelope.hash,"signature":envelope.signature,"version":event.version,
            "space_id":event.network_id,"scope_id":event.scope_id,"resource_id":event.resource_id,"author":event.author,
            "sequence":event.sequence,"previous":event.previous,"parents":event.parents,"created_at_ms":event.created_at_ms,
            "admission_hash":ObjectHash::body(admission)?,"body_hash":ObjectHash::body(&event.body)?});
        self.objects
            .insert(ObjectHash::body(&metadata)?, canonical(&metadata)?.len());
        self.signatures.insert(ObjectHash::body(&(
            "vibeface:event:v2",
            &event.author,
            &envelope.hash,
            &envelope.signature,
        ))?);
        Ok(())
    }
    pub fn record_handoff(&mut self, wire: &SignedExecutionHandoff) -> Result<()> {
        self.record_signed(&wire.invocation)?;
        self.record_capsule(&wire.handoff)
    }
    fn record_capsule(&mut self, handoff: &ExecutionHandoff) -> Result<()> {
        self.record_event(&handoff.task_creation)?;
        self.record_event(&handoff.project_dispatch)?;
        if let Some(source) = &handoff.source_attestation {
            self.record_signed(source)?;
        }
        for (current, origin) in [
            (true, &handoff.sdk_origin),
            (false, &handoff.task_sdk_origin),
        ] {
            let Some(origin) = origin else { continue };
            require(
                origin.receipts.len() <= MAX_POLICY_DOMAINS
                    && origin.challenge.policies.len() <= MAX_POLICY_DOMAINS,
                "too many historical SDK origin items",
            )?;
            if current {
                self.record_signed(&origin.invocation)?;
            } else {
                self.record_historical_wrapper(&origin.invocation)?;
            }
            for receipt in &origin.receipts {
                self.record_historical_wrapper(receipt)?;
            }
            self.record_metadata(&serde_json::json!({"message_event":origin.message_event,"sdk_trigger":origin.sdk_trigger,
                "challenge":origin.challenge,"admitted_at_ms":origin.admitted_at_ms,"transport":origin.transport}))?;
        }
        self.record_metadata(&serde_json::json!({"version":handoff.version,"execution_resource":handoff.execution_resource,
            "execution_audience":handoff.execution_audience,"target_public_key":handoff.target_public_key}))
    }
    pub fn record_historical_handoff(&mut self, wire: &SignedExecutionHandoff) -> Result<()> {
        let mut historical = Self::default();
        historical.record_handoff(wire)?;
        self.metadata.extend(
            historical
                .objects
                .iter()
                .map(|(hash, size)| (hash.clone(), *size)),
        );
        self.historical_wrappers.extend(
            historical
                .objects
                .iter()
                .map(|(hash, size)| (hash.clone(), *size)),
        );
        self.historical_wrappers
            .extend(historical.historical_wrappers);
        self.metadata.extend(historical.metadata);
        self.task_payloads.extend(historical.task_payloads);
        Ok(())
    }
    pub fn counts(&self) -> ExecutionProofCounts {
        ExecutionProofCounts {
            authorization_bytes: self.objects.values().sum::<usize>()
                + self
                    .metadata
                    .iter()
                    .filter(|(hash, _)| !self.objects.contains_key(*hash))
                    .map(|(_, size)| *size)
                    .sum::<usize>(),
            unique_signed_objects: self.objects.len(),
            unique_signatures: self.signatures.len(),
            max_chain_depth: self.max_chain_depth,
            historical_wrappers: self
                .historical_wrappers
                .keys()
                .filter(|hash| !self.objects.contains_key(*hash))
                .count(),
            historical_wrapper_bytes: self
                .historical_wrappers
                .iter()
                .filter(|(hash, _)| !self.objects.contains_key(*hash))
                .map(|(_, size)| *size)
                .sum(),
            task_payload_bytes: self.task_payloads.values().sum(),
            actual: self.actual.clone(),
        }
    }
    pub fn enforce_limits(&self) -> Result<()> {
        let counts = self.counts();
        if counts.authorization_bytes > MAX_PROOF_BYTES
            || counts.unique_signed_objects > MAX_PROOF_OBJECTS
            || counts.unique_signatures > MAX_PROOF_SIGNATURES
            || counts.max_chain_depth > MAX_DELEGATION_DEPTH
        {
            return Err(AuthorizationError::BudgetExceeded);
        }
        Ok(())
    }
}
