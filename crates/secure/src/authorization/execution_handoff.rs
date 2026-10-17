use super::{events::*, task_binding::*, *};
use serde_json::Value;

pub const MAX_EXECUTION_HANDOFF_BYTES: usize = 8 * 1024 * 1024;
pub const MAX_EXECUTION_HANDOFF_EVENTS: usize = 256;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ExecutionHandoff {
    pub version: u16,
    pub project_dispatch: SignedEvent,
    pub project_history: Vec<SignedEvent>,
    pub execution_resource: ResourceRef,
    pub execution_audience: Audience,
    pub target_public_key: PublicKey,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct SignedExecutionHandoff {
    pub handoff: ExecutionHandoff,
    pub invocation: Signed<Invocation>,
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
                    && matches!(stage, CheckStage::Start | CheckStage::Renew)),
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
}

impl VerifiedExecutionHandoff {
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
}

impl VerifiedExecutionFreshness {
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
        handoff.version == AUTH_VERSION
            && handoff.project_history.len() <= MAX_EXECUTION_HANDOFF_EVENTS
            && handoff
                .project_history
                .windows(2)
                .all(|pair| pair[0].hash < pair[1].hash)
            && handoff
                .project_history
                .iter()
                .all(|event| event.hash != handoff.project_dispatch.hash),
        "invalid execution handoff event set",
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
    pub fn verify_execution_handoff(
        &self,
        wire: &SignedExecutionHandoff,
        original: &VerifiedAuthorization,
        transport: &TransportEvidence,
        admitted: Option<&VerifiedExecutionAdmission>,
        now_ms: u64,
    ) -> Result<VerifiedExecutionHandoff> {
        self.check_current(original, now_ms)?;
        let hash = wire.hash()?;
        let facts = execution_submission_facts(&wire.handoff, original)?;
        let (event, _, request, binding) = original_dispatch(&wire.handoff)?;
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
        Ok(VerifiedExecutionFreshness {
            handoff_hash: handoff.hash.clone(),
            challenge_hash: hash,
            nonce: challenge.nonce.clone(),
            stage: challenge.stage,
            dependencies: handoff.dependencies.clone(),
            verified_at_ms: now_ms,
            expires_at_ms,
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
