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
        let original_context = original_request.context()?;
        require(
            matches!(
                verified.facts.action,
                Action::OperationStatusRead | Action::OperationResolve | Action::OperationReconcile
            ) && original_request.expected_action() == verified.facts.action,
            "original SDK History read has no readonly child source",
        )?;
        require(
            verified.facts.context.task == original_context.context.task
                && verified.facts.context.conversation == original_context.context.conversation,
            "original SDK History read changed its source Task or conversation",
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const NOW: u64 = 10_000;

    fn key(byte: u8) -> Identity {
        Identity::from_secret_hex(&hex::encode([byte; 32])).unwrap()
    }

    fn admission(
        state: &VerifiedPolicySet,
        invocation: Signed<Invocation>,
        source_body: serde_json::Value,
        transport: TransportEvidence,
        node: &Identity,
        nonce: u8,
    ) -> (events::AdmissionProof, VerifiedAuthorization) {
        let facts = invocation_facts(&invocation.payload);
        let authorization = state
            .verify_invocation(&invocation, &facts, &transport, NOW)
            .unwrap();
        let challenge = FreshnessChallenge::new(
            &authorization,
            CheckStage::Admit,
            Nonce::from_bytes([nonce; 32]),
            NOW,
            NOW + 5_000,
        )
        .unwrap();
        let receipts: Vec<_> = authorization
            .dependencies()
            .iter()
            .map(|current| {
                Signed::sign(
                    FreshnessReceipt {
                        version: AUTH_VERSION,
                        current: current.clone(),
                        challenge_hash: challenge.hash().unwrap(),
                        issued_at_ms: NOW,
                        expires_at_ms: NOW + 5_000,
                    },
                    node,
                )
                .unwrap()
            })
            .collect();
        let freshness = state
            .verify_freshness(&authorization, &challenge, &receipts, NOW)
            .unwrap();
        let admitted = state
            .admit_operation(&authorization, &freshness, NOW)
            .unwrap();
        let current = state
            .verify_operation_authorization(&admitted, &facts, &transport, NOW + 20)
            .unwrap();
        (
            events::AdmissionProof {
                invocation,
                source_body,
                challenge,
                receipts,
                admitted_at_ms: NOW,
                transport: events::StoredTransport::from(&transport),
            },
            current,
        )
    }

    fn fixture() -> (
        VerifiedPolicySet,
        SdkPolicySourceProvenance,
        VerifiedAuthorization,
        NodeExecutionPolicyReadRequest,
        Identity,
    ) {
        let root = key(1);
        let recovery = key(2);
        let user = key(3);
        let node = key(4);
        let nonce = Nonce::from_bytes([5; 32]);
        let claim =
            bootstrap::create_node_claim(&node, nonce.as_str(), NOW as f64, (NOW + 10_000) as f64)
                .unwrap();
        let claim = bootstrap::verify_node_claim(
            &claim,
            &node.public_key_hex(),
            nonce.as_str(),
            NOW as f64,
        )
        .unwrap();
        let prepared = bootstrap::prepare_account_creation(
            &root,
            &recovery,
            &user,
            &claim,
            (NOW + 1_000_000) as f64,
            NOW as f64,
        )
        .unwrap();
        let possession = bootstrap::sign_node_possession(
            &prepared.node_request_json().unwrap(),
            &node,
            nonce.as_str(),
            NOW as f64,
        )
        .unwrap();
        let created =
            bootstrap::finish_account_creation(&prepared, &root, &possession, NOW as f64).unwrap();
        let bundle: ClientAuthorizationBundle =
            serde_json::from_str(&created.bundle_json().unwrap()).unwrap();
        let pins: Vec<PolicyPin> = serde_json::from_str(&created.pins_json().unwrap()).unwrap();
        let state = replay_policies(&pins, &bundle.journal).unwrap();
        let service_grant: Signed<Grant> =
            serde_json::from_str(&created.service_grant_json().unwrap()).unwrap();
        let service_grant = service_grant.hash().unwrap();
        let resource = state
            .resources
            .values()
            .next()
            .map(|record| ResourceRef {
                space: record.payload.space.clone(),
                resource: record.payload.resource.clone(),
            })
            .unwrap();
        let audience = Audience {
            service: ServiceId::from_key(&PublicKey::of(&node)),
            device: DeviceId::from_key(&PublicKey::of(&node)),
        };
        let context = InvocationContext {
            conversation: Some("chat".into()),
            workspace_resource: Some(resource.resource.clone()),
            ..Default::default()
        };
        let actor = state
            .find_device(&DeviceId::from_key(&PublicKey::of(&user)))
            .unwrap()
            .payload
            .subject
            .clone();
        let mut heads = vec![
            state
                .head(&PolicyDomain::Account(match &actor {
                    Actor::Account(id) => id.clone(),
                    _ => panic!("user has no Account"),
                }))
                .unwrap()
                .clone(),
            state
                .head(&PolicyDomain::Space(resource.space.clone()))
                .unwrap()
                .clone(),
        ];
        heads.sort_by(|a, b| a.domain.cmp(&b.domain));
        let device = DeviceId::from_key(&PublicKey::of(&user));
        let source_body = json!({"message":"run this"});
        let invocation = Signed::sign(
            Invocation {
                version: AUTH_VERSION,
                actor: actor.clone(),
                device: device.clone(),
                initiator: actor,
                initiating_grant: None,
                resource: resource.clone(),
                action: Action::MessageAppend,
                audience: audience.clone(),
                presenter: device.clone(),
                operation_id: "parent".into(),
                body_hash: ObjectHash::body(&source_body).unwrap(),
                grant_leaf: bundle.grant_leaf,
                expected_policies: heads,
                context: context.clone(),
                nonce: Nonce::from_bytes([6; 32]),
                not_before_ms: NOW,
                expires_at_ms: NOW + 10,
            },
            &user,
        )
        .unwrap();
        let (origin, parent) = admission(
            &state,
            invocation,
            source_body,
            TransportEvidence::DirectNoise { device },
            &node,
            7,
        );
        let trigger = EventRef::parse(&"aa".repeat(32)).unwrap();
        let sdk = SessionRef {
            target: audience.device.clone(),
            session_id: trigger.as_str().into(),
        };
        let manager_body = json!({"method":"agent_start","space_id":resource.space,"resource_id":resource.resource,
            "conversation_id":"chat","authorization_id":format!("manager:{}",trigger.as_str()),"trigger":trigger,
            "sdk_session":sdk,"parent_invocation":parent.invocation_hash(),"sdk_config":"fixture"});
        let mut manager_context = context;
        manager_context.sdk_session = Some(sdk.clone());
        let manager_facts = RequestFacts {
            resource: resource.clone(),
            action: Action::AgentStart,
            audience: audience.clone(),
            operation_id: format!("manager:{}", trigger.as_str()),
            body_hash: ObjectHash::body(&manager_body).unwrap(),
            context: manager_context,
        };
        let delegation = state
            .verify_service_delegation(
                &parent,
                &audience.service,
                &service_grant,
                &manager_facts,
                NOW + 20,
            )
            .unwrap();
        let invocation = Signed::sign(
            Invocation {
                version: AUTH_VERSION,
                actor: Actor::Service(audience.service.clone()),
                device: delegation.device().clone(),
                initiator: delegation.initiator().clone(),
                initiating_grant: delegation.initiating_grant().cloned(),
                resource: resource.clone(),
                action: Action::AgentStart,
                audience: audience.clone(),
                presenter: delegation.device().clone(),
                operation_id: manager_facts.operation_id.clone(),
                body_hash: manager_facts.body_hash.clone(),
                grant_leaf: service_grant.clone(),
                expected_policies: delegation.heads().to_vec(),
                context: manager_facts.context.clone(),
                nonce: Nonce::from_bytes([8; 32]),
                not_before_ms: NOW,
                expires_at_ms: NOW + 1_000,
            },
            &node,
        )
        .unwrap();
        let (manager, _) = admission(
            &state,
            invocation,
            manager_body,
            TransportEvidence::LocalAgentAdapter {
                service: audience.service.clone(),
                session: sdk.clone(),
            },
            &node,
            9,
        );
        let mut child = manager_facts;
        child.action = Action::ExecutionOpen;
        child.operation_id = "child-open".into();
        child.body_hash = ObjectHash::body(&json!({"open":true})).unwrap();
        child.context.task = Some(EventRef::parse(&"bb".repeat(32)).unwrap());
        child.context.executor_kind = Some("codex".into());
        let target = PublicKey::of(&key(20));
        child.context.execution_session = Some(SessionRef {
            target: DeviceId::from_key(&target),
            session_id: "foreign-session".into(),
        });
        let heads = sdk_policy_source_heads(
            &state,
            &parent,
            &audience.service,
            &service_grant,
            &child,
            NOW + 20,
        )
        .unwrap();
        let record = SdkPolicySourceProvenance {
            version: AUTH_VERSION,
            origin: execution_handoff::CompactSdkOrigin {
                message_event: trigger.clone(),
                sdk_trigger: trigger,
                invocation: origin.invocation,
                challenge: origin.challenge,
                receipts: origin.receipts,
                admitted_at_ms: origin.admitted_at_ms,
                transport: origin.transport,
            },
            session: SdkPolicySession {
                space_id: resource.space.clone(),
                resource_id: resource.resource.clone(),
                service: audience.service,
                device: audience.device,
                conversation_id: "chat".into(),
                sdk_session: sdk,
                grant_leaf: service_grant.clone(),
                initiator: delegation.initiator().clone(),
                initiating_grant: delegation.initiating_grant().cloned(),
                parent_invocation: parent.invocation_hash().clone(),
                expected_policies: delegation.heads().to_vec(),
                expires_at_ms: delegation.expires_at_ms(),
            },
            service_grant,
            lease: SdkPolicyLease {
                challenge: manager.challenge.clone(),
                receipts: manager.receipts.clone(),
                checked_at_ms: NOW,
                deadline: NOW + 1_000,
            },
            manager_admission: manager,
            current_binding: None,
            child_facts: SdkPolicySourceFacts::from(&child),
            source_heads: heads,
        };
        let mut read_context = child.context;
        read_context.sdk_session = None;
        let request = NodeExecutionPolicyReadRequest::Current(
            Signed::sign(
                NodeExecutionPolicyReadIntent {
                    version: AUTH_VERSION,
                    source_project: resource.clone(),
                    selection: RemoteExecutionPolicySelection {
                        execution_resource: resource,
                        target_public_key: target.clone(),
                        execution_grant: record.service_grant.clone(),
                    },
                    context: prepare::PrepareContext {
                        context: read_context,
                        binding_generation: Some(1),
                        canonical_cwd: Some("/workspace".into()),
                        execution_target_public_key: Some(target),
                    },
                    expected_action: Action::ExecutionOpen,
                    purpose: RemoteExecutionPolicyPurpose::Execution,
                    execution_pins: pins,
                    source_provenance: Some(ObjectHash::body(&record).unwrap()),
                    nonce: Nonce::from_bytes([10; 32]),
                    issued_at_ms: NOW + 20,
                    expires_at_ms: NOW + 200,
                },
                &node,
            )
            .unwrap(),
        );
        (state, record, parent, request, node)
    }

    #[test]
    fn original_sdk_source_survives_account_window_and_rejects_changed_material() {
        let (state, record, parent, request, _) = fixture();
        assert!(parent.signed_invocation().payload.expires_at_ms < NOW + 20);
        let facts = record.child_facts.request_facts();
        let current = verify_sdk_source_provenance(
            &state,
            &parent,
            &record,
            &record.session.service,
            &record.service_grant,
            &facts,
            None,
            NOW + 20,
        )
        .unwrap();
        assert_eq!(current.facts(), &facts);
        assert_eq!(current.heads(), record.source_heads);
        assert_eq!(current.digest(), &ObjectHash::body(&record).unwrap());
        let restored = restore_sdk_source_provenance(&state, &record, &request, NOW + 20).unwrap();
        assert_eq!(
            restored.parent().invocation_hash(),
            parent.invocation_hash()
        );
        assert!(restored.proof_usage().counts().unique_signed_objects > 0);
        let mut changed = record.clone();
        changed.child_facts.context.task = Some(EventRef::parse(&"cc".repeat(32)).unwrap());
        assert!(restore_sdk_source_provenance(&state, &changed, &request, NOW + 20).is_err());
        let mut changed = record.clone();
        changed.manager_admission.source_body["sdk_config"] = json!("another-manager");
        assert!(
            verify_sdk_source_provenance(
                &state,
                &parent,
                &changed,
                &record.session.service,
                &record.service_grant,
                &facts,
                None,
                NOW + 20
            )
            .is_err()
        );
        let mut changed = record.clone();
        changed.lease.deadline += 1;
        assert!(
            verify_sdk_source_provenance(
                &state,
                &parent,
                &changed,
                &record.session.service,
                &record.service_grant,
                &facts,
                None,
                NOW + 20
            )
            .is_err()
        );
        let mut changed = record.clone();
        changed.lease.checked_at_ms += 1;
        assert!(
            verify_sdk_source_provenance(
                &state,
                &parent,
                &changed,
                &record.session.service,
                &record.service_grant,
                &facts,
                None,
                NOW + 20
            )
            .is_err()
        );
        assert!(restore_sdk_source_provenance(&state, &record, &request, NOW + 200).is_err());
    }

    #[test]
    fn signed_node_read_cannot_omit_or_substitute_the_sdk_source_digest() {
        let (state, record, _, request, node) = fixture();
        let NodeExecutionPolicyReadRequest::Current(value) = request else {
            unreachable!()
        };
        let mut payload = value.payload;
        payload.source_provenance = None;
        let missing =
            NodeExecutionPolicyReadRequest::Current(Signed::sign(payload.clone(), &node).unwrap());
        assert!(restore_sdk_source_provenance(&state, &record, &missing, NOW + 20).is_err());
        payload.source_provenance = Some(ObjectHash::body(&"unrelated").unwrap());
        let wrong = NodeExecutionPolicyReadRequest::Current(Signed::sign(payload, &node).unwrap());
        assert!(restore_sdk_source_provenance(&state, &record, &wrong, NOW + 20).is_err());
    }

    #[test]
    fn renewed_manager_source_outlives_its_original_invocation_window() {
        let (state, mut record, parent, request, node) = fixture();
        let facts = invocation_facts(&record.manager_admission.invocation.payload);
        let mut usage = ExecutionProofUsage::default();
        let admitted =
            manager_admission(&state, &record.manager_admission, &facts, &mut usage).unwrap();
        let checked = NOW + 1_500;
        assert!(record.manager_admission.invocation.payload.expires_at_ms < checked);
        let manager = state
            .verify_operation_authorization(
                &admitted,
                &facts,
                &record.manager_admission.transport.clone().into_evidence(),
                checked,
            )
            .unwrap();
        let challenge = FreshnessChallenge::new(
            &manager,
            CheckStage::Renew,
            Nonce::from_bytes([11; 32]),
            checked,
            checked + 1_000,
        )
        .unwrap();
        let receipts = manager
            .dependencies()
            .iter()
            .map(|current| {
                Signed::sign(
                    FreshnessReceipt {
                        version: AUTH_VERSION,
                        current: current.clone(),
                        challenge_hash: challenge.hash().unwrap(),
                        issued_at_ms: checked,
                        expires_at_ms: checked + 1_000,
                    },
                    &node,
                )
                .unwrap()
            })
            .collect();
        record.lease = SdkPolicyLease {
            challenge,
            receipts,
            checked_at_ms: checked,
            deadline: checked + 1_000,
        };
        let child = record.child_facts.request_facts();
        let current = verify_sdk_source_provenance(
            &state,
            &parent,
            &record,
            &record.session.service,
            &record.service_grant,
            &child,
            None,
            checked + 10,
        )
        .unwrap();
        assert_eq!(current.expires_at_ms(), checked + 1_000);
        let NodeExecutionPolicyReadRequest::Current(value) = request else {
            unreachable!()
        };
        let mut payload = value.payload;
        payload.issued_at_ms = checked + 10;
        payload.expires_at_ms = checked + 200;
        payload.source_provenance = Some(ObjectHash::body(&record).unwrap());
        let request =
            NodeExecutionPolicyReadRequest::Current(Signed::sign(payload, &node).unwrap());
        assert!(restore_sdk_source_provenance(&state, &record, &request, checked + 10).is_ok());
        record.lease.receipts[0] = record.origin.receipts[0].clone();
        assert!(
            verify_sdk_source_provenance(
                &state,
                &parent,
                &record,
                &record.session.service,
                &record.service_grant,
                &child,
                None,
                checked + 10
            )
            .is_err()
        );
    }

    #[test]
    fn original_source_restores_exact_old_heads_after_current_grant_revocation() {
        let (mut state, record, parent, request, _) = fixture();
        let domain = PolicyDomain::Space(record.session.space_id.clone());
        let mut document = state.space(&record.session.space_id).unwrap().clone();
        document.previous = state.head(&domain).cloned();
        document.sequence += 1;
        document
            .grants
            .retain(|grant| grant != &record.service_grant);
        state
            .install_space(Signed::sign(document, &key(1)).unwrap())
            .unwrap();
        let facts = record.child_facts.request_facts();
        assert!(
            verify_sdk_source_provenance(
                &state,
                &parent,
                &record,
                &record.session.service,
                &record.service_grant,
                &facts,
                None,
                NOW + 20
            )
            .is_err()
        );
        let original = restore_sdk_source_provenance(&state, &record, &request, NOW + 20).unwrap();
        assert_eq!(original.heads(), record.source_heads);
        assert_ne!(original.state().head(&domain), state.head(&domain));
        let mut changed = record.clone();
        changed.source_heads.pop();
        let NodeExecutionPolicyReadRequest::Current(value) = request else {
            unreachable!()
        };
        let mut payload = value.payload;
        payload.source_provenance = Some(ObjectHash::body(&changed).unwrap());
        let changed_request =
            NodeExecutionPolicyReadRequest::Current(Signed::sign(payload, &key(4)).unwrap());
        assert!(
            restore_sdk_source_provenance(&state, &changed, &changed_request, NOW + 20).is_err()
        );
    }
}
