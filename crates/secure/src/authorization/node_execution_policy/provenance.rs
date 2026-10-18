//! Original SDK source material bound to a signed Node policy read.

use super::*;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SdkPolicySession {
    pub space_id: SpaceId,
    pub resource_id: ResourceId,
    pub service: ServiceId,
    pub device: DeviceId,
    pub conversation_id: String,
    pub sdk_session: SessionRef,
    pub grant_leaf: ObjectHash,
    pub initiator: Actor,
    pub initiating_grant: Option<ObjectHash>,
    pub parent_invocation: ObjectHash,
    pub expected_policies: Vec<PolicyStamp>,
    pub expires_at_ms: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SdkPolicyLease {
    pub challenge: FreshnessChallenge,
    pub receipts: Vec<Signed<FreshnessReceipt>>,
    pub checked_at_ms: u64,
    pub deadline: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SdkPolicySourceFacts {
    pub resource: ResourceRef,
    pub action: Action,
    pub audience: Audience,
    pub operation_id: String,
    pub body_hash: ObjectHash,
    pub context: InvocationContext,
}

impl From<&RequestFacts> for SdkPolicySourceFacts {
    fn from(facts: &RequestFacts) -> Self {
        Self {
            resource: facts.resource.clone(),
            action: facts.action,
            audience: facts.audience.clone(),
            operation_id: facts.operation_id.clone(),
            body_hash: facts.body_hash.clone(),
            context: facts.context.clone(),
        }
    }
}

impl SdkPolicySourceFacts {
    pub fn request_facts(&self) -> RequestFacts {
        RequestFacts {
            resource: self.resource.clone(),
            action: self.action,
            audience: self.audience.clone(),
            operation_id: self.operation_id.clone(),
            body_hash: self.body_hash.clone(),
            context: self.context.clone(),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct SdkPolicySourceProvenance {
    pub version: u16,
    pub origin: execution_handoff::CompactSdkOrigin,
    pub session: SdkPolicySession,
    pub service_grant: ObjectHash,
    pub manager_admission: events::AdmissionProof,
    pub lease: SdkPolicyLease,
    pub current_binding: Option<SignedBinding>,
    pub child_facts: SdkPolicySourceFacts,
    pub source_heads: Vec<PolicyStamp>,
}

pub fn sdk_policy_source_heads(
    primary: &VerifiedPolicySet,
    parent: &VerifiedAuthorization,
    service: &ServiceId,
    service_grant: &ObjectHash,
    child_facts: &RequestFacts,
    now: u64,
) -> Result<Vec<PolicyStamp>> {
    primary
        .verify_sdk_policy_read_source(parent, service, service_grant, child_facts, now)
        .map(|source| source.heads().to_vec())
}

/// Read-only source authority; it does not authorize an execution invocation.
#[derive(Clone)]
pub struct VerifiedSdkSourceProvenance {
    state: VerifiedPolicySet,
    parent: VerifiedAuthorization,
    facts: RequestFacts,
    heads: Vec<PolicyStamp>,
    usage: ExecutionProofUsage,
    expires_at_ms: u64,
    digest: ObjectHash,
}

impl VerifiedSdkSourceProvenance {
    pub fn state(&self) -> &VerifiedPolicySet {
        &self.state
    }
    pub fn parent(&self) -> &VerifiedAuthorization {
        &self.parent
    }
    pub fn facts(&self) -> &RequestFacts {
        &self.facts
    }
    pub fn heads(&self) -> &[PolicyStamp] {
        &self.heads
    }
    pub fn proof_usage(&self) -> &ExecutionProofUsage {
        &self.usage
    }
    pub fn expires_at_ms(&self) -> u64 {
        self.expires_at_ms
    }
    pub fn digest(&self) -> &ObjectHash {
        &self.digest
    }
}

fn invocation_facts(invocation: &Invocation) -> RequestFacts {
    RequestFacts {
        resource: invocation.resource.clone(),
        action: invocation.action,
        audience: invocation.audience.clone(),
        operation_id: invocation.operation_id.clone(),
        body_hash: invocation.body_hash.clone(),
        context: invocation.context.clone(),
    }
}

fn bounded_heads(heads: &[PolicyStamp]) -> Result<()> {
    require(
        !heads.is_empty()
            && heads.len() <= MAX_POLICY_DOMAINS
            && heads.windows(2).all(|pair| pair[0].domain < pair[1].domain),
        "SDK source policy heads are missing, duplicated or unordered",
    )
}

fn origin_parent(
    primary: &VerifiedPolicySet,
    record: &SdkPolicySourceProvenance,
    now: u64,
    usage: &mut ExecutionProofUsage,
) -> Result<VerifiedAuthorization> {
    let origin = &record.origin;
    let value = &origin.invocation.payload;
    require(
        matches!(value.actor, Actor::Account(_))
            && value.initiator == value.actor
            && value.initiating_grant.is_none()
            && value.action == Action::MessageAppend
            && value.context.sdk_session.is_none()
            && value.context.execution_session.is_none()
            && origin.admitted_at_ms <= now
            && origin.challenge.stage == CheckStage::Admit,
        "SDK source has no original Account message admission",
    )?;
    let facts = invocation_facts(value);
    let transport = origin.transport.clone().into_evidence();
    let historical = remote_resolution_history::at_heads(primary, &origin.challenge.policies)?;
    let authorization = historical.verify_invocation(
        &origin.invocation,
        &facts,
        &transport,
        origin.admitted_at_ms,
    )?;
    let fresh = historical.verify_freshness(
        &authorization,
        &origin.challenge,
        &origin.receipts,
        origin.admitted_at_ms,
    )?;
    let admission = historical.admit_operation(&authorization, &fresh, origin.admitted_at_ms)?;
    usage.record_authorization(&authorization)?;
    usage.record_signed(&origin.invocation)?;
    for receipt in &origin.receipts {
        usage.record_signed(receipt)?;
    }
    usage.record_metadata(&serde_json::json!({
        "message_event":origin.message_event, "sdk_trigger":origin.sdk_trigger,
        "challenge":origin.challenge, "admitted_at_ms":origin.admitted_at_ms,
        "transport":origin.transport,
    }))?;
    primary.verify_operation_authorization(&admission, &facts, &transport, now)
}

fn manager_shape(
    record: &SdkPolicySourceProvenance,
    parent: &VerifiedAuthorization,
) -> Result<RequestFacts> {
    let session = &record.session;
    let proof = &record.manager_admission;
    let value = &proof.invocation.payload;
    let body = &proof.source_body;
    let trigger = record.origin.sdk_trigger.as_str();
    let sdk: SessionRef = serde_json::from_value(body["sdk_session"].clone())
        .map_err(|_| AuthorizationError::InvalidProof("invalid SDK manager session"))?;
    require(
        body["method"] == "agent_start"
            && body["space_id"].as_str() == Some(session.space_id.as_str())
            && body["resource_id"].as_str() == Some(session.resource_id.as_str())
            && body["conversation_id"].as_str() == Some(session.conversation_id.as_str())
            && body["trigger"].as_str() == Some(trigger)
            && body["authorization_id"].as_str() == Some(value.operation_id.as_str())
            && body["parent_invocation"].as_str() == Some(session.parent_invocation.as_str())
            && body["sdk_config"]
                .as_str()
                .is_some_and(|config| !config.is_empty())
            && ObjectHash::body(body)? == value.body_hash
            && value.operation_id == format!("manager:{trigger}")
            && value.action == Action::AgentStart
            && value.actor == Actor::Service(session.service.clone())
            && value.device == session.device
            && value.initiator == session.initiator
            && value.initiating_grant == session.initiating_grant
            && value.grant_leaf == session.grant_leaf
            && value.resource
                == ResourceRef {
                    space: session.space_id.clone(),
                    resource: session.resource_id.clone(),
                }
            && value.context.conversation.as_deref() == Some(session.conversation_id.as_str())
            && value.context.workspace_resource.as_ref() == Some(&session.resource_id)
            && value.context.sdk_session.as_ref() == Some(&session.sdk_session)
            && value.context.execution_session.is_none()
            && sdk == session.sdk_session
            && sdk.target == session.device
            && sdk.session_id == trigger
            && proof.transport.clone().into_evidence()
                == TransportEvidence::LocalAgentAdapter {
                    service: session.service.clone(),
                    session: session.sdk_session.clone(),
                }
            && parent.invocation_hash() == &session.parent_invocation
            && parent.actor() == &session.initiator
            && parent.resource() == &value.resource
            && parent.audience() == &value.audience
            && parent.signed_invocation() == &record.origin.invocation
            && record.origin.admitted_at_ms <= proof.admitted_at_ms,
        "SDK manager admission differs from its original session or Account parent",
    )?;
    Ok(invocation_facts(value))
}

fn verify_lease(
    primary: &VerifiedPolicySet,
    record: &SdkPolicySourceProvenance,
    admission: &VerifiedAdmission,
    manager_facts: &RequestFacts,
    now: u64,
    usage: &mut ExecutionProofUsage,
) -> Result<()> {
    let lease = &record.lease;
    let proof = &record.manager_admission;
    require(
        proof.admitted_at_ms <= lease.checked_at_ms
            && lease.checked_at_ms <= now
            && now < lease.deadline
            && matches!(lease.challenge.stage, CheckStage::Admit | CheckStage::Renew),
        "SDK manager lease is outside its original interval",
    )?;
    let view = remote_resolution_history::at_heads(primary, &lease.challenge.policies)?;
    let transport = proof.transport.clone().into_evidence();
    let authorization = if lease.challenge.stage == CheckStage::Admit {
        require(
            lease.checked_at_ms == proof.admitted_at_ms
                && lease.challenge == proof.challenge
                && lease.receipts == proof.receipts,
            "SDK initial lease differs from its manager admission",
        )?;
        view.verify_invocation(
            &proof.invocation,
            manager_facts,
            &transport,
            lease.checked_at_ms,
        )?
    } else {
        view.verify_operation_authorization(
            admission,
            manager_facts,
            &transport,
            lease.checked_at_ms,
        )?
    };
    let fresh = view.verify_freshness(
        &authorization,
        &lease.challenge,
        &lease.receipts,
        lease.checked_at_ms,
    )?;
    require(
        lease.deadline == fresh.expires_at_ms().min(authorization.expires_at_ms()),
        "SDK manager lease deadline differs from its signed receipts",
    )?;
    usage.record_authorization(&authorization)?;
    usage.record_metadata(&serde_json::json!({
        "challenge":lease.challenge,"checked_at_ms":lease.checked_at_ms,"deadline":lease.deadline,
    }))?;
    for receipt in &lease.receipts {
        usage.record_signed(receipt)?;
    }
    Ok(())
}

fn manager_admission(
    primary: &VerifiedPolicySet,
    proof: &events::AdmissionProof,
    facts: &RequestFacts,
    usage: &mut ExecutionProofUsage,
) -> Result<VerifiedAdmission> {
    let historical = remote_resolution_history::at_heads(primary, &proof.challenge.policies)?;
    let authorization = historical.verify_invocation(
        &proof.invocation,
        facts,
        &proof.transport.clone().into_evidence(),
        proof.admitted_at_ms,
    )?;
    let freshness = historical.verify_freshness(
        &authorization,
        &proof.challenge,
        &proof.receipts,
        proof.admitted_at_ms,
    )?;
    usage.record_authorization(&authorization)?;
    historical.admit_operation(&authorization, &freshness, proof.admitted_at_ms)
}

fn verify_source(
    primary: &VerifiedPolicySet,
    record: &SdkPolicySourceProvenance,
    now: u64,
) -> Result<VerifiedSdkSourceProvenance> {
    let _signatures = ScopedSignatureVerification::new();
    require(
        record.version == AUTH_VERSION,
        "unsupported SDK source provenance version",
    )?;
    bounded_heads(&record.source_heads)?;
    bounded_heads(&record.session.expected_policies)?;
    require(
        record.origin.receipts.len() <= MAX_POLICY_DOMAINS
            && record.manager_admission.receipts.len() <= MAX_POLICY_DOMAINS
            && record.lease.receipts.len() <= MAX_POLICY_DOMAINS,
        "too many SDK source freshness receipts",
    )?;
    let mut usage = ExecutionProofUsage::default();
    let parent = origin_parent(primary, record, now, &mut usage)?;
    let manager_facts = manager_shape(record, &parent)?;
    let proof = &record.manager_admission;
    require(
        proof.admitted_at_ms <= now,
        "SDK manager admission clock moved backwards",
    )?;
    let admitted = manager_admission(primary, proof, &manager_facts, &mut usage)?;
    let manager = primary.verify_operation_authorization(
        &admitted,
        &manager_facts,
        &proof.transport.clone().into_evidence(),
        now,
    )?;
    let delegation = primary.verify_service_delegation(
        &parent,
        &record.session.service,
        &record.session.grant_leaf,
        &manager_facts,
        now,
    )?;
    require(
        delegation.device() == &record.session.device
            && delegation.initiator() == &record.session.initiator
            && delegation.initiating_grant() == record.session.initiating_grant.as_ref()
            && now < record.session.expires_at_ms,
        "SDK manager has no current service delegation",
    )?;
    usage.record_authorization(&parent)?;
    usage.record_authorization(&manager)?;
    usage.record_admission(proof)?;
    verify_lease(primary, record, &admitted, &manager_facts, now, &mut usage)?;
    let facts = record.child_facts.request_facts();
    require(
        facts.resource == manager_facts.resource
            && facts.audience == manager_facts.audience
            && facts.context.conversation == manager_facts.context.conversation
            && facts.context.sdk_session.as_ref() == Some(&record.session.sdk_session)
            && facts.context.workspace_resource.as_ref() == Some(&record.session.resource_id)
            && manager_facts
                .context
                .task
                .as_ref()
                .is_none_or(|task| facts.context.task.as_ref() == Some(task)),
        "SDK policy child changed its manager session, Task or Project",
    )?;
    let source = primary.verify_sdk_policy_read_source(
        &parent,
        &record.session.service,
        &record.service_grant,
        &facts,
        now,
    )?;
    require(
        source.service() == &record.session.service
            && source.device() == &record.session.device
            && source.heads() == record.source_heads
            && manager
                .dependencies()
                .iter()
                .all(|head| record.source_heads.contains(head))
            && delegation
                .heads()
                .iter()
                .all(|head| record.source_heads.contains(head)),
        "SDK policy source differs from its genuine current heads",
    )?;
    // Descriptor heads are retained metadata, never a source of policy authority.
    remote_resolution_history::at_heads(primary, &record.session.expected_policies)?;
    usage.merge(source.proof_usage());
    if let Some(binding) = &record.current_binding {
        let key = &primary
            .find_device(&record.session.device)?
            .payload
            .public_key;
        task_binding::verify_binding(binding, key.as_str()).map_err(|_| {
            AuthorizationError::InvalidProof("SDK source binding has another signer")
        })?;
        require(
            binding.binding.network_id == facts.resource.space.as_str()
                && facts
                    .context
                    .task
                    .as_ref()
                    .is_some_and(|task| task.as_str() == binding.binding.task_ref)
                && facts.context.conversation.as_deref()
                    == Some(binding.binding.conversation_id.as_str()),
            "SDK source binding belongs to another Task or conversation",
        )?;
        usage.record_binding(binding)?;
    }
    usage.record_metadata(&serde_json::json!({
        "version":record.version,"session":record.session,"service_grant":record.service_grant,
        "child_facts":record.child_facts,"source_heads":record.source_heads,
        "current_binding_hash":record.current_binding.as_ref().map(ObjectHash::body).transpose()?,
    }))?;
    if let Some(counts) = crate::signature_verification::SignatureVerification::current_counts() {
        usage.record_actual_signature_counts(counts);
    }
    usage.enforce_limits()?;
    Ok(VerifiedSdkSourceProvenance {
        state: primary.clone(),
        parent,
        facts,
        heads: source.heads().to_vec(),
        usage,
        expires_at_ms: source
            .expires_at_ms()
            .min(delegation.expires_at_ms())
            .min(record.lease.deadline)
            .min(record.session.expires_at_ms),
        digest: ObjectHash::body(record)?,
    })
}

pub fn verify_sdk_source_provenance(
    primary: &VerifiedPolicySet,
    parent: &VerifiedAuthorization,
    record: &SdkPolicySourceProvenance,
    service: &ServiceId,
    service_grant: &ObjectHash,
    child_facts: &RequestFacts,
    current_binding: Option<&SignedBinding>,
    now: u64,
) -> Result<VerifiedSdkSourceProvenance> {
    require(
        &record.session.service == service
            && &record.service_grant == service_grant
            && record.child_facts == SdkPolicySourceFacts::from(child_facts)
            && record.current_binding.as_ref() == current_binding,
        "SDK source provenance changed its current transaction inputs",
    )?;
    primary.check_current(parent, now)?;
    let verified = verify_source(primary, record, now)?;
    require(
        verified.parent().signed_invocation() == parent.signed_invocation()
            && verified.parent().dependencies() == parent.dependencies(),
        "SDK source provenance changed its verified Account parent",
    )?;
    Ok(verified)
}

/// Rechecks the original signed read and original policy view; it grants no current writer.
pub fn restore_sdk_source_provenance(
    primary: &VerifiedPolicySet,
    record: &SdkPolicySourceProvenance,
    original_request: &NodeExecutionPolicyReadRequest,
    original_clock: u64,
) -> Result<VerifiedSdkSourceProvenance> {
    let _signatures = ScopedSignatureVerification::new();
    require(
        canonical(original_request)?.len() <= MAX_PROOF_BYTES,
        "original SDK Node read exceeds its carrier budget",
    )?;
    read_window(
        original_request.issued_at_ms(),
        original_request.expires_at_ms(),
        original_clock,
    )?;
    validate_pins(original_request.execution_pins())?;
    match original_request {
        NodeExecutionPolicyReadRequest::Current(value) => {
            value.verify_signature()?;
            require(
                value.payload.version == AUTH_VERSION
                    && value.payload.purpose != RemoteExecutionPolicyPurpose::History,
                "original SDK Node Current read changed its version or purpose",
            )?;
        }
        NodeExecutionPolicyReadRequest::History(value) => {
            value.verify_signature()?;
            require(
                value.payload.version == AUTH_VERSION,
                "original SDK Node History read changed its version",
            )?;
            remote_execution_history::validate_remote_execution_history_reference(
                &value.payload.reference,
            )?;
        }
    }
    let digest = ObjectHash::body(record)?;
    require(
        original_request.source_provenance() == Some(&digest)
            && ServiceId::from_key(original_request.issuer()) == record.session.service
            && DeviceId::from_key(original_request.issuer()) == record.session.device
            && original_request.source_project() == &record.child_facts.resource,
        "original Node policy read changed its complete SDK source provenance",
    )?;
    let source = remote_resolution_history::at_heads(primary, &record.source_heads)?;
    let mut verified = verify_source(&source, record, original_clock)?;
    let mut context = verified.facts.context.clone();
    context.sdk_session = None;
    if let NodeExecutionPolicyReadRequest::Current(value) = original_request {
        require(
            value.payload.context.context == context
                && value.payload.expected_action
                    == match value.payload.purpose {
                        RemoteExecutionPolicyPurpose::Execution => verified.facts.action,
                        RemoteExecutionPolicyPurpose::ObservationRead
                        | RemoteExecutionPolicyPurpose::Resolver => {
                            require(
                                matches!(
                                    verified.facts.action,
                                    Action::OperationStatusRead
                                        | Action::OperationResolve
                                        | Action::OperationReconcile
                                ),
                                "original SDK policy read has no readonly child source",
                            )?;
                            Action::OperationResolve
                        }
                        RemoteExecutionPolicyPurpose::History => {
                            return Err(AuthorizationError::InvalidProof(
                                "current SDK source cannot claim History",
                            ));
                        }
                    },
            "original SDK policy read changed its complete child context or action",
        )?;
    } else {
        require(
            matches!(
                verified.facts.action,
                Action::OperationStatusRead | Action::OperationResolve | Action::OperationReconcile
            ) && original_request.expected_action() == verified.facts.action,
            "original SDK History read has no readonly child source",
        )?;
    }
    require(
        original_request.expires_at_ms() <= verified.expires_at_ms,
        "original Node read exceeds its SDK source authority",
    )?;
    match original_request {
        NodeExecutionPolicyReadRequest::Current(value) => verified.usage.record_signed(value)?,
        NodeExecutionPolicyReadRequest::History(value) => verified.usage.record_signed(value)?,
    }
    if let Some(counts) = crate::signature_verification::SignatureVerification::current_counts() {
        verified.usage.record_actual_signature_counts(counts);
    }
    verified.usage.enforce_limits()?;
    Ok(verified)
}
