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
    pub conversation: String,
    pub target_public_key: PublicKey,
    pub operation_id: String,
    pub original_invocation_hash: ObjectHash,
    pub original_admission_hash: ObjectHash,
    pub work: ExecutionWork,
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
    original: VerifiedAdmission,
    sdk_parent: Option<VerifiedAdmission>,
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
        require(
            self.original.admitted_at_ms() <= now_ms,
            "Project source admission is from the future",
        )?;
        Ok(self.original.clone())
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
        require(
            origin.admitted_at_ms <= now_ms,
            "SDK parent admission is from the future",
        )?;
        let parent = self
            .sdk_parent
            .as_ref()
            .ok_or(AuthorizationError::InvalidProof(
                "Project source has no verified SDK parent admission",
            ))?;
        require(
            parent.signed_invocation() == &origin.invocation
                && parent.admitted_at_ms() == origin.admitted_at_ms,
            "Project source changed its verified SDK parent",
        )?;
        Ok(Some(parent.clone()))
    }
    fn check_capsule(&self, handoff: &ExecutionHandoff) -> Result<()> {
        require(
            handoff.source_statement()?.capsule_hash == self.capsule_hash,
            "Project source checkpoint belongs to another capsule",
        )
    }
}

fn admission_facts(invocation: &Invocation) -> RequestFacts {
    RequestFacts {
        resource: invocation.resource.clone(),
        action: invocation.action,
        audience: invocation.audience.clone(),
        operation_id: invocation.operation_id.clone(),
        body_hash: invocation.body_hash.clone(),
        context: invocation.context.clone(),
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ExecutionOrigin {
    TaskSession {
        task_creation: SignedEvent,
    },
    ConversationOpen,
    ConversationSession {
        accepted_open: Box<ExecutionHandoffResult>,
    },
    Command,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ExecutionHandoff {
    pub version: u16,
    pub project_dispatch: SignedEvent,
    pub origin: ExecutionOrigin,
    pub sdk_origin: Option<CompactSdkOrigin>,
    pub task_sdk_origin: Option<CompactSdkOrigin>,
    pub execution_resource: ResourceRef,
    pub execution_audience: Audience,
    pub target_public_key: PublicKey,
    pub source_attestation: Option<Signed<ProjectExecutionSource>>,
}

impl ExecutionHandoff {
    pub fn source_statement(&self) -> Result<ProjectExecutionSource> {
        let (event, proof, _, work) = original_dispatch(self)?;
        #[derive(Serialize)]
        struct Capsule<'a> {
            version: u16,
            project_dispatch: &'a SignedEvent,
            origin: &'a ExecutionOrigin,
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
                origin: &self.origin,
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
            conversation: event.scope_id.clone(),
            target_public_key: self.target_public_key.clone(),
            operation_id: proof.invocation.payload.operation_id.clone(),
            original_invocation_hash: proof.invocation.hash()?,
            original_admission_hash: ObjectHash::body(&proof)?,
            work,
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

#[derive(Clone, Debug)]
pub struct VerifiedExecutionHandoffResult {
    result: ExecutionHandoffResult,
    authorization: VerifiedExecutionHandoff,
}

impl VerifiedExecutionHandoffResult {
    pub fn result(&self) -> &ExecutionHandoffResult {
        &self.result
    }
    pub fn original(&self) -> &VerifiedAuthorization {
        self.authorization.original()
    }
    pub fn submission(&self) -> &VerifiedAuthorization {
        self.authorization.submission()
    }
    pub fn dependencies(&self) -> &[PolicyStamp] {
        self.authorization.dependencies()
    }
    pub fn proof_usage(&self) -> &ExecutionProofUsage {
        self.authorization.proof_usage()
    }
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
    pub work: ExecutionWork,
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

    pub(super) fn bound_to(
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
            work: handoff.work.clone(),
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
            && transport.existing_evidence()?
                == TransportEvidence::VerifiedPeer {
                    origin: wire.invocation.payload.presenter.clone(),
                }
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
        submission: VerifiedAdmission::from_execution_checkpoint(
            wire.invocation.clone(),
            admitted_at_ms,
            transport.existing_evidence()?,
            usage.clone(),
            admission
                .receipts
                .iter()
                .map(|value| value.payload.expires_at_ms)
                .fold(
                    admission
                        .challenge
                        .expires_at_ms
                        .min(wire.invocation.payload.expires_at_ms),
                    u64::min,
                ),
        )?,
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
    work: ExecutionWork,
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
    pub fn work(&self) -> &ExecutionWork {
        &self.work
    }
    pub fn binding(&self) -> Result<SignedBinding> {
        self.wire.handoff.task_binding()
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

pub(super) fn check_execution_source_work(
    leaf: &Grant,
    project: &ResourceRef,
    source_node: &PublicKey,
    work: &ExecutionWork,
) -> Result<()> {
    match &leaf.execution_source {
        Some(source) => {
            require(
                source.project == *project && source.node_public_key == *source_node,
                "execution grant changed its source Project",
            )?;
            source.work.matches_work(work)
        }
        None => require(
            matches!(work, ExecutionWork::TaskSession { .. }),
            "ordinary execution requires an owner-approved work source",
        ),
    }
}

pub use super::execution_work::{
    ProjectDispatchOrigin, ProjectExecutionReceipt, ProjectExecutionResult,
    derive_project_dispatch_work, verify_project_execution_receipt_signature,
};

pub(super) fn original_dispatch(
    handoff: &ExecutionHandoff,
) -> Result<(Event, AdmissionProof, Value, ExecutionWork)> {
    require(
        handoff.version == AUTH_VERSION
            && handoff.execution_audience.device == DeviceId::from_key(&handoff.target_public_key)
            && handoff.execution_audience.service
                == ServiceId::from_key(&handoff.target_public_key),
        "invalid execution handoff version or audience",
    )?;
    let event: Event =
        serde_json::from_str(&handoff.project_dispatch.canonical_body).map_err(invalid_shape)?;
    let (admission, request) =
        execution_work::project_dispatch_request(&event, &handoff.target_public_key)?;
    if let ExecutionOrigin::ConversationSession { accepted_open } = &handoff.origin {
        require(
            accepted_open.handoff.handoff.execution_resource == handoff.execution_resource
                && accepted_open.handoff.handoff.execution_audience == handoff.execution_audience,
            "conversation operation changed its accepted Open resource",
        )?;
    }
    let work = execution_work::derive_remote_dispatch_work(
        &event,
        &handoff.origin,
        &handoff.target_public_key,
    )?;
    Ok((event.clone(), admission.clone(), request.clone(), work))
}

fn required_text(value: &Value, max: usize) -> Result<String> {
    let value = value
        .as_str()
        .filter(|v| !v.is_empty() && v.len() <= max)
        .ok_or(AuthorizationError::InvalidProof(
            "invalid execution work text",
        ))?;
    Ok(value.to_owned())
}

impl ExecutionHandoff {
    pub fn task_binding(&self) -> Result<SignedBinding> {
        require(
            matches!(self.origin, ExecutionOrigin::TaskSession { .. }),
            "execution work has no Task binding",
        )?;
        let (_, _, request, _) = original_dispatch(self)?;
        serde_json::from_value(request["binding"].clone()).map_err(invalid_shape)
    }

    pub fn execution_context(&self) -> Result<InvocationContext> {
        let (event, proof, _, work) = original_dispatch(self)?;
        let executor_kind = match &work {
            ExecutionWork::TaskSession { .. } => self.task_binding()?.binding.open_spec["executor"]
                .as_str()
                .map(str::to_owned),
            ExecutionWork::ConversationSession { executor, .. } => Some(executor.clone()),
            ExecutionWork::Command { .. } => None,
        };
        Ok(InvocationContext {
            conversation: Some(event.scope_id),
            task: work.task_ref().cloned(),
            sdk_session: proof.invocation.payload.context.sdk_session.clone(),
            execution_session: work.execution_session().cloned(),
            executor_kind,
            workspace_resource: Some(event.resource_id),
        })
    }
}

pub fn execution_submission_facts(
    handoff: &ExecutionHandoff,
    original: &VerifiedAuthorization,
) -> Result<RequestFacts> {
    let (event, proof, request, work) = original_dispatch(handoff)?;
    require(
        proof.invocation == *original.signed_invocation()
            && original.phase == AuthorizationPhase::Operation
            && original.action() == execution_action(&request)?
            && *original.body_hash() == ObjectHash::body(&proof.source_body)?
            && original.resource().space.as_str() == event.network_id
            && original.resource().resource == event.resource_id
            && request["operation_id"].as_str() == Some(original.operation_id())
            && original.signed_invocation().payload.context == handoff.execution_context()?,
        "handoff has a different original authorization",
    )?;
    let mut context = original.signed_invocation().payload.context.clone();
    require(
        work.execution_session()
            .is_none_or(|session| session.target == handoff.execution_audience.device),
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

pub(super) fn execution_action(request: &Value) -> Result<Action> {
    Ok(match request["action"]["type"].as_str() {
        Some("command") => Action::ExecutionCommand,
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
                    && proof.transport.existing_evidence()?
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
            let parent =
                self.verify_admitted_operation_authorization(&parent, &parent_facts, now_ms)?;
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
        let (dispatch, proof, _, work) = original_dispatch(handoff)?;
        require(
            matches!(handoff.origin, ExecutionOrigin::TaskSession { .. })
                || handoff.task_sdk_origin.is_none(),
            "ordinary execution cannot carry a Task origin",
        )?;
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
        events::verify_signed_event(
            &serde_json::to_string(&handoff.project_dispatch).map_err(invalid_shape)?,
            statement.project.space.as_str(),
            statement.project.resource.as_str(),
            certificate.payload.public_key.as_str(),
        )
        .map_err(invalid_shape)?;
        require(
            dispatch.blobs.is_empty(),
            "execution dispatch cannot carry blobs",
        )?;
        let mut usage = ExecutionProofUsage::default();
        usage.record_budget(&source_budget)?;
        usage.record_signed(signed)?;
        match &handoff.origin {
            ExecutionOrigin::TaskSession { task_creation } => {
                let binding = handoff.task_binding()?;
                let task = events::verify_signed_event(
                    &serde_json::to_string(task_creation).map_err(invalid_shape)?,
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
                    binding.binding.task_ref == task_creation.hash
                        && binding.binding.task_id == *task_id
                        && binding.binding.authority_node == task.event().author
                        && task.event().scope_id == dispatch.scope_id
                        && task.event().blobs.is_empty(),
                    "execution capsule belongs to another task",
                )?;
                verify_binding(&binding, certificate.payload.public_key.as_str())
                    .map_err(invalid_shape)?;
                let EventCause::Invocation {
                    admission: task_admission,
                } = &task.event().cause
                else {
                    return Err(AuthorizationError::InvalidProof(
                        "Task source has no admission",
                    ));
                };
                usage.record_event_signature(task_creation, task.event(), task_admission)?;
                usage.record_binding(&binding)?;
            }
            ExecutionOrigin::ConversationSession { accepted_open } => {
                require(
                    matches!(
                        accepted_open.handoff.handoff.origin,
                        ExecutionOrigin::ConversationOpen
                    ),
                    "conversation source is not an initial Open",
                )?;
                let accepted_source = self
                    .verify_execution_observation_source(&accepted_open.handoff.handoff, now_ms)?;
                require(
                    accepted_open.handoff.handoff.source_statement()?.work == work,
                    "conversation source changed its accepted Open",
                )?;
                super::execution_observation::ExecutionObservationTarget::from_original(
                    &accepted_open.handoff,
                )?;
                usage.merge(accepted_source.proof_usage());
                usage.record_signed(&accepted_open.receipt)?;
            }
            ExecutionOrigin::ConversationOpen | ExecutionOrigin::Command => {
                require(
                    handoff.task_sdk_origin.is_none(),
                    "non-Task execution cannot carry a Task origin",
                )?;
            }
        }
        usage.record_event_signature(&handoff.project_dispatch, &dispatch, &proof)?;
        let original =
            self.verify_original_admission(&proof, &admission_facts(&proof.invocation.payload))?;
        usage.merge(original.proof_usage());
        usage.enforce_limits()?;
        let mut verified = VerifiedProjectExecutionSource {
            capsule_hash: statement.capsule_hash,
            usage,
            original,
            sdk_parent: None,
        };
        verified.sdk_parent =
            self.verify_compact_sdk_parent_admission(&verified, handoff, now_ms)?;
        if let Some(parent) = &verified.sdk_parent {
            verified.usage.merge(parent.proof_usage());
            verified.usage.enforce_limits()?;
        }
        verified.original_admission(handoff, now_ms)?;
        verified.sdk_parent_admission(handoff, now_ms)?;
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
        let (event, original_proof, request, work) = original_dispatch(&wire.handoff)?;
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
        if let ExecutionWork::TaskSession { .. } = &work {
            let binding = wire.handoff.task_binding()?;
            verify_binding(&binding, project_device.payload.public_key.as_str())
                .map_err(invalid_shape)?;
            require(
                binding.binding.authority_node == event.author,
                "handoff binding is not from its Project authority",
            )?;
        }
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
                            | Action::ExecutionCommand
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
        require(
            super::prepare::remote_context_matches(
                &facts.context,
                &facts.audience.device,
                &leaf.actions,
            ),
            "execution grant combines incompatible work capabilities",
        )?;
        check_execution_source_work(leaf, original.resource(), &wire.invocation.issuer, &work)?;
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
            work,
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
        let hash = challenge.hash()?;
        let mut seen = BTreeSet::new();
        let mut budget = Budget::default();
        let mut expires_at_ms = challenge.expires_at_ms.min(handoff.expires_at_ms());
        for signed in &proof.receipts {
            budget.verify(signed)?;
            let receipt = &signed.payload;
            receipt.validate_currents()?;
            require(
                receipt.version == AUTH_VERSION
                    && receipt.challenge_hash == hash
                    && receipt.issued_at_ms >= challenge.issued_at_ms
                    && receipt.expires_at_ms <= challenge.expires_at_ms,
                "handoff freshness receipt mismatch",
            )?;
            validate_window(receipt.issued_at_ms, receipt.expires_at_ms, now_ms)?;
            for current in &receipt.currents {
                require(
                    handoff.dependencies.contains(current)
                        && self.head(&current.domain) == Some(current)
                        && seen.insert(current.domain.clone()),
                    "handoff freshness receipt mismatch",
                )?;
                let authority = match &current.domain {
                    PolicyDomain::Account(id) => self.account(id).map(|d| &d.authority),
                    PolicyDomain::Space(id) => self.space(id).map(|d| &d.authority),
                }
                .ok_or(AuthorizationError::InvalidProof(
                    "missing handoff freshness authority",
                ))?;
                if self
                    .devices
                    .values()
                    .any(|certificate| certificate.payload.device == authority.device)
                {
                    let certificate = self.find_device(&authority.device)?;
                    require(
                        certificate.payload.public_key == signed.issuer
                            && certificate.payload.subject
                                == Actor::Service(authority.service.clone()),
                        "handoff receipt is not from its current authority",
                    )?;
                }
                require(
                    signed.issuer == authority.public_key
                        && DeviceId::from_key(&signed.issuer) == authority.device
                        && ServiceId::from_key(&signed.issuer) == authority.service,
                    "handoff receipt is not from its current authority",
                )?;
            }
            expires_at_ms = expires_at_ms.min(receipt.expires_at_ms);
        }
        require(
            seen.len() == handoff.dependencies.len(),
            "handoff requires fresh receipts from both authority closures",
        )?;
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
            submission: VerifiedAdmission::from_execution_checkpoint(
                handoff.submission.invocation.clone(),
                now_ms,
                TransportEvidence::VerifiedPeer {
                    origin: handoff.submission.device().clone(),
                },
                freshness.usage.clone(),
                freshness.expires_at_ms,
            )?,
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
    pub work: ExecutionWork,
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
pub struct ExecutionPendingRequest {
    pub request_id: Value,
    pub turn_id: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ExecutionControlState {
    pub turn_id: Option<String>,
    pub pending_requests: Vec<ExecutionPendingRequest>,
    pub expires_at_ms: u64,
}

impl ExecutionControlState {
    pub fn validate_for_session(
        &self,
        session: &ExecutionSessionSnapshot,
        issued_at_ms: u64,
    ) -> Result<()> {
        require(
            session.work.execution_session().is_some()
                && self.expires_at_ms > issued_at_ms
                && self.expires_at_ms <= session.expires_at_ms
                && canonical(self)?.len() <= MAX_PROOF_BYTES,
            "execution control has no live session lease",
        )?;
        if let Some(turn) = &self.turn_id {
            require(
                !turn.is_empty() && turn.len() <= 512,
                "invalid execution control turn",
            )?;
        }
        let mut requests = BTreeSet::new();
        for pending in &self.pending_requests {
            let valid_id = match &pending.request_id {
                Value::String(value) => value.len() <= 512,
                Value::Number(value) => value
                    .as_i64()
                    .is_some_and(|value| value.unsigned_abs() <= 9_007_199_254_740_991),
                _ => false,
            };
            require(
                valid_id
                    && self.turn_id.as_ref() == Some(&pending.turn_id)
                    && requests.insert(ObjectHash::body(&pending.request_id)?),
                "execution control has an invalid request id or another turn",
            )?;
        }
        Ok(())
    }
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
    pub work: ExecutionWork,
    pub state: ExecutionHandoffState,
    pub result_hash: ObjectHash,
    pub session: Option<ExecutionSessionSnapshot>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub control: Option<ExecutionControlState>,
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
    wire.invocation.verify_signature()?;
    let (event, proof, _, work) = original_dispatch(&wire.handoff)?;
    let value = &receipt.payload;
    if let Some(session) = &value.session {
        verify_snapshot_work(session, &wire.handoff, &work)?;
        require(
            session.expires_at_ms > value.issued_at_ms
                && session.expires_at_ms - value.issued_at_ms <= EXECUTION_LEASE_MS,
            "executor session snapshot has an invalid lease",
        )?;
    }
    if let Some(control) = &value.control {
        require(
            canonical(receipt)?.len() <= MAX_PROOF_BYTES,
            "execution control receipt exceeds its proof budget",
        )?;
        let session = value
            .session
            .as_ref()
            .ok_or(AuthorizationError::InvalidProof(
                "execution control requires a session snapshot",
            ))?;
        control.validate_for_session(session, value.issued_at_ms)?;
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
            && value.work == work
            && value.result_hash == ObjectHash::body(result)?,
        "execution handoff result signature mismatch",
    )
}

pub fn sign_execution_handoff_receipt_with_control(
    wire: &SignedExecutionHandoff,
    mut receipt: ExecutionHandoffReceipt,
    result: &Value,
    control: Option<ExecutionControlState>,
    signer: &Identity,
) -> Result<Signed<ExecutionHandoffReceipt>> {
    require(
        PublicKey::of(signer) == wire.handoff.target_public_key
            && receipt.result_hash == ObjectHash::body(result)?,
        "execution control changed its executor or durable result",
    )?;
    receipt.control = control;
    let receipt = Signed::sign(receipt, signer)?;
    verify_execution_handoff_receipt_signature(wire, &receipt, result)?;
    Ok(receipt)
}

pub fn verify_execution_control_state<'a>(
    wire: &SignedExecutionHandoff,
    receipt: &'a Signed<ExecutionHandoffReceipt>,
    result: &Value,
    expected_revision: u64,
    now_ms: u64,
) -> Result<&'a ExecutionControlState> {
    verify_execution_handoff_receipt_signature(wire, receipt, result)?;
    let session = receipt
        .payload
        .session
        .as_ref()
        .ok_or(AuthorizationError::InvalidProof(
            "execution control requires a session snapshot",
        ))?;
    let control = receipt
        .payload
        .control
        .as_ref()
        .ok_or(AuthorizationError::InvalidProof(
            "executor has no observed control state",
        ))?;
    require(
        expected_revision <= 9_007_199_254_740_991
            && session.revision == expected_revision
            && receipt.payload.issued_at_ms >= wire.invocation.payload.not_before_ms
            && receipt.payload.issued_at_ms <= now_ms
            && now_ms < control.expires_at_ms,
        "execution control revision or observation lease changed",
    )?;
    Ok(control)
}

fn verify_snapshot_work(
    session: &ExecutionSessionSnapshot,
    handoff: &ExecutionHandoff,
    work: &ExecutionWork,
) -> Result<()> {
    let source = handoff.source_statement()?;
    let expected_session = work
        .execution_session()
        .ok_or(AuthorizationError::InvalidProof(
            "command cannot carry a session snapshot",
        ))?;
    let (executor, cwd) = match work {
        ExecutionWork::TaskSession { .. } => {
            let binding = handoff.task_binding()?;
            (
                required_text(&binding.binding.open_spec["executor"], 128)?,
                required_text(&binding.binding.open_spec["cwd"], 4096)?,
            )
        }
        ExecutionWork::ConversationSession {
            executor,
            canonical_cwd,
            ..
        } => (executor.clone(), canonical_cwd.clone()),
        ExecutionWork::Command { .. } => unreachable!(),
    };
    require(
        session.original_project == source.project
            && session.execution_resource == handoff.execution_resource
            && session.work == *work
            && session.target == expected_session.target
            && session.session_id == expected_session.session_id
            && session.executor == executor
            && session.cwd == cwd
            && !session.state.is_empty()
            && session.state.len() <= 128,
        "signed executor session snapshot differs from its original work",
    )
}

impl VerifiedPolicySet {
    pub fn verify_execution_handoff_result(
        &self,
        result: &ExecutionHandoffResult,
        original: &VerifiedAuthorization,
        now_ms: u64,
    ) -> Result<VerifiedExecutionHandoffResult> {
        let authorization = self.verify_execution_handoff_ack(
            &result.handoff,
            original,
            &result.receipt,
            &result.result,
            now_ms,
        )?;
        Ok(VerifiedExecutionHandoffResult {
            result: result.clone(),
            authorization,
        })
    }

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
            submission: VerifiedAdmission::from_execution_checkpoint(
                wire.invocation.clone(),
                receipt.payload.issued_at_ms,
                TransportEvidence::VerifiedPeer {
                    origin: wire.invocation.payload.presenter.clone(),
                },
                usage.clone(),
                0,
            )?,
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
                && value.work == handoff.work
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
            && value.work == session.work
            && session.original_project == *project
            && session.execution_resource == *execution
            && binding.binding.network_id == project.space.as_str()
            && binding.binding.authority_node
                == node_fingerprint(project_key.as_str()).map_err(invalid_shape)?
            && session.work
                == ExecutionWork::TaskSession {
                    task: EventRef::parse(&binding.binding.task_ref)?,
                    binding_hash: binding_hash(&binding.binding).map_err(invalid_shape)?,
                    binding_generation: binding.binding.generation,
                    execution_session: SessionRef {
                        target: value.target.clone(),
                        session_id: binding.binding.session_id.clone(),
                    },
                }
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
    let (event, original, request, work) = original_dispatch(&unknown.handoff.handoff)?;
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
            && body["task_ref"] == serde_json::to_value(work.task_ref()).map_err(invalid_shape)?
            && body["target_node"] == request["target_node"]
            && body["session_id"] == request["session_id"]
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
    let mut context = unknown.handoff.handoff.execution_context()?;
    context.sdk_session = None;
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
            && transport.existing_evidence()?
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
        submission: VerifiedAdmission::from_execution_checkpoint(
            result.resolution.invocation.clone(),
            resolved_at_ms,
            transport.existing_evidence()?,
            usage.clone(),
            freshness
                .receipts
                .iter()
                .map(|value| value.payload.expires_at_ms)
                .fold(
                    freshness
                        .challenge
                        .expires_at_ms
                        .min(invocation.expires_at_ms),
                    u64::min,
                ),
        )?,
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

    pub(super) fn bound_to(
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
        let admitted = self.verify_original_admission(proof, &facts)?;
        self.verify_admitted_operation_authorization(&admitted, &facts, now_ms)
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
            "executor resolution grant must bind only explicit resolution and this work",
        )?;
        let source = wire
            .resolution
            .observation
            .handoff
            .handoff
            .source_statement()?;
        check_execution_source_work(leaf, &source.project, &wire.invocation.issuer, &source.work)?;
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
        let hash = challenge.hash()?;
        let mut seen = BTreeSet::new();
        let mut budget = Budget::default();
        let mut expiry = challenge.expires_at_ms.min(value.expires_at_ms());
        for signed in &proof.receipts {
            budget.verify(signed)?;
            let receipt = &signed.payload;
            receipt.validate_currents()?;
            require(
                receipt.version == AUTH_VERSION
                    && receipt.challenge_hash == hash
                    && receipt.issued_at_ms >= challenge.issued_at_ms
                    && receipt.expires_at_ms <= challenge.expires_at_ms,
                "resolution freshness receipt mismatch",
            )?;
            validate_window(receipt.issued_at_ms, receipt.expires_at_ms, now_ms)?;
            for current in &receipt.currents {
                require(
                    value.dependencies.contains(current)
                        && self.head(&current.domain) == Some(current)
                        && seen.insert(current.domain.clone()),
                    "resolution freshness receipt mismatch",
                )?;
                let authority = match &current.domain {
                    PolicyDomain::Account(id) => self.account(id).map(|d| &d.authority),
                    PolicyDomain::Space(id) => self.space(id).map(|d| &d.authority),
                }
                .ok_or(AuthorizationError::InvalidProof(
                    "missing resolution authority",
                ))?;
                if self
                    .devices
                    .values()
                    .any(|certificate| certificate.payload.device == authority.device)
                {
                    let certificate = self.find_device(&authority.device)?;
                    require(
                        certificate.payload.public_key == signed.issuer
                            && certificate.payload.subject
                                == Actor::Service(authority.service.clone()),
                        "resolution receipt is not from its current authority",
                    )?;
                }
                require(
                    signed.issuer == authority.public_key
                        && DeviceId::from_key(&signed.issuer) == authority.device
                        && ServiceId::from_key(&signed.issuer) == authority.service,
                    "resolution receipt is not from its current authority",
                )?;
            }
            expiry = expiry.min(receipt.expires_at_ms);
        }
        require(
            seen.len() == value.dependencies.len(),
            "resolution requires both current authority closures",
        )?;
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
        let deadline = self.verify_execution_resolution_freshness(value, proof, now_ms)?;
        require(
            value.submission.phase == AuthorizationPhase::Admission,
            "resolution was already admitted",
        )?;
        Ok(VerifiedExecutionResolutionAdmission {
            resolution_hash: value.hash.clone(),
            submission: VerifiedAdmission::from_execution_checkpoint(
                value.wire.invocation.clone(),
                now_ms,
                TransportEvidence::VerifiedPeer {
                    origin: value.wire.invocation.payload.presenter.clone(),
                },
                value.usage.clone(),
                deadline,
            )?,
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
            submission: VerifiedAdmission::from_execution_checkpoint(
                result.resolution.invocation.clone(),
                result.receipt.payload.issued_at_ms,
                TransportEvidence::VerifiedPeer {
                    origin: result.resolution.invocation.payload.presenter.clone(),
                },
                usage.clone(),
                0,
            )?,
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
    pub fn record_cached_admission(
        &mut self,
        auth: &VerifiedAuthorization,
        proof: &AdmissionProof,
    ) -> Result<()> {
        require(
            auth.signed_invocation() == &proof.invocation,
            "cached admission differs from its verified invocation",
        )?;
        self.objects.extend(
            auth.budget
                .object_bytes
                .iter()
                .map(|(hash, size)| (hash.clone(), *size)),
        );
        self.metadata.extend(
            auth.budget
                .metadata
                .iter()
                .map(|(hash, size)| (hash.clone(), *size)),
        );
        self.record_chain_depth(auth.grant_path().len());
        let mut retained = Self::default();
        retained.record_admission_kind(proof, false)?;
        retained.signatures.clear();
        self.merge(&retained);
        Ok(())
    }
    pub(super) fn record_budget(&mut self, budget: &Budget) -> Result<()> {
        for (hash, size) in &budget.object_bytes {
            self.objects.insert(hash.clone(), *size);
        }
        self.metadata.extend(
            budget
                .metadata
                .iter()
                .map(|(hash, size)| (hash.clone(), *size)),
        );
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
    pub(crate) fn record_verified_object<T: SignedObject>(
        &mut self,
        value: &Signed<T>,
    ) -> Result<()> {
        self.objects.insert(value.hash()?, canonical(value)?.len());
        Ok(())
    }
    pub fn record_metadata<T: Serialize>(&mut self, value: &T) -> Result<()> {
        self.metadata
            .insert(ObjectHash::body(value)?, canonical(value)?.len());
        Ok(())
    }
    pub(super) fn normalize_completed_task_policy_transport(
        &mut self,
        current: &node_execution_policy::VerifiedTaskCurrentPolicy,
    ) -> Result<()> {
        fn closed_carrier<T: SignedObject>(
            usage: &mut ExecutionProofUsage,
            value: &Signed<T>,
        ) -> Result<()> {
            let hash = value.hash()?;
            let bytes = canonical(value)?.len();
            if let Some(recorded) = usage.objects.remove(&hash) {
                require(
                    recorded == bytes,
                    "completed Task policy carrier changed its recorded bytes",
                )?;
            }
            usage.metadata.remove(&hash);
            usage.historical_wrappers.insert(hash.clone(), bytes);
            usage.record_metadata(&serde_json::json!({
                "completed_task_policy_carrier": {
                    "domain": T::DOMAIN,
                    "hash": hash,
                    "envelope_hash": ObjectHash::body(value)?,
                    "issuer": value.issuer,
                    "encoded_bytes": bytes,
                },
            }))
        }

        // Current retains the completed reads and their exact originals; its approval stays live.
        closed_carrier(self, current.trust().original_intent())?;
        match current.request() {
            node_execution_policy::TaskExecutionPolicyRequest::Account(request) => {
                closed_carrier(self, &request.intent)?;
                closed_carrier(self, &request.delegation)?;
            }
            node_execution_policy::TaskExecutionPolicyRequest::Node(request) => match request {
                node_execution_policy::NodeExecutionPolicyReadRequest::Current(value) => {
                    closed_carrier(self, value)?;
                }
                node_execution_policy::NodeExecutionPolicyReadRequest::History(_) => {
                    return Err(AuthorizationError::InvalidProof(
                        "completed Current cannot carry a History request",
                    ));
                }
            },
        }
        closed_carrier(self, current.snapshot())?;
        closed_carrier(self, current.witness())
    }
    pub(super) fn normalize_history_acceptance_metadata(
        &mut self,
        freshness: &ExecutionHandoffFreshness,
    ) -> Result<()> {
        let proof_hash = ObjectHash::body(freshness)?;
        let mut receipt_hashes = Vec::with_capacity(freshness.receipts.len());
        for receipt in &freshness.receipts {
            self.record_signed(receipt)?;
            receipt_hashes.push(ObjectHash::body(receipt)?);
        }
        self.metadata.remove(&proof_hash);
        self.record_metadata(&serde_json::json!({
            "history_acceptance_hash": proof_hash,
            "challenge": freshness.challenge,
            "receipt_hashes": receipt_hashes,
        }))
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
    fn transport_metadata(&mut self, transport: &StoredTransport) -> Result<Value> {
        match transport {
            StoredTransport::DirectedGateway { header } => {
                self.record_signed(header)?;
                Ok(serde_json::json!({"kind":"directed_gateway","header_hash":header.hash()?}))
            }
            _ => serde_json::to_value(transport).map_err(invalid_shape),
        }
    }
    fn record_admission_kind(&mut self, proof: &AdmissionProof, checkpointed: bool) -> Result<()> {
        require(
            proof.receipts.len() <= MAX_POLICY_DOMAINS
                && proof.challenge.policies.len() <= MAX_POLICY_DOMAINS,
            "too many historical admission items",
        )?;
        let mut source = proof.source_body.clone();
        if source["method"] == "message"
            && let Some(text) = source["text"].as_str()
        {
            source["text"] = serde_json::json!({
                "payload_hash":ObjectHash::body(&text)?,"utf8_bytes":text.len(),
            });
        }
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
            if !source["request"]["binding"].is_null() {
                let binding: SignedBinding =
                    serde_json::from_value(source["request"]["binding"].clone())
                        .map_err(invalid_shape)?;
                self.record_binding(&binding)?;
                source["request"]["binding"] =
                    serde_json::json!({"signed_binding_hash":ObjectHash::body(&binding)?});
            }
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
        let transport = self.transport_metadata(&proof.transport)?;
        self.record_metadata(&serde_json::json!({"source_body_hash":ObjectHash::body(&proof.source_body)?,"source_metadata":source,
            "challenge":proof.challenge,"admitted_at_ms":proof.admitted_at_ms,"transport":transport}))
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
    pub fn record_retained_event(&mut self, envelope: &SignedEvent) -> Result<()> {
        let event: Event = serde_json::from_str(&envelope.canonical_body).map_err(invalid_shape)?;
        let EventCause::Invocation { admission } = &event.cause else {
            return Err(AuthorizationError::InvalidProof(
                "retained event has no original admission",
            ));
        };
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
        match &handoff.origin {
            ExecutionOrigin::TaskSession { task_creation } => self.record_event(task_creation)?,
            ExecutionOrigin::ConversationSession { accepted_open } => {
                require(
                    matches!(
                        accepted_open.handoff.handoff.origin,
                        ExecutionOrigin::ConversationOpen
                    ),
                    "execution capsule contains nested session origins",
                )?;
                self.record_handoff(&accepted_open.handoff)?;
                self.record_signed(&accepted_open.receipt)?;
                self.record_metadata(&accepted_open.result)?;
            }
            ExecutionOrigin::ConversationOpen | ExecutionOrigin::Command => {}
        }
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
            let transport = self.transport_metadata(&origin.transport)?;
            self.record_metadata(&serde_json::json!({"message_event":origin.message_event,"sdk_trigger":origin.sdk_trigger,
                "challenge":origin.challenge,"admitted_at_ms":origin.admitted_at_ms,"transport":transport}))?;
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
