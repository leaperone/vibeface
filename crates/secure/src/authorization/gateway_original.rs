//! Accepted request provenance cannot be promoted into a live transport.

use super::{
    events::{AdmissionProof, StoredTransport},
    execution_handoff::ExecutionProofUsage,
    gateway_forward::{GatewayForwardHeader, GatewayOriginal},
    *,
};
use crate::signature_verification::{ScopedSignatureVerification, SignatureVerification};

#[derive(Clone, Debug)]
pub(super) struct AdmittedDestination {
    pub(super) resource: ResourceRef,
    pub(super) public_key: PublicKey,
    pub(super) grant_leaf: ObjectHash,
}

#[derive(Clone, Debug)]
pub(super) enum AcceptedGatewayOriginal {
    Full(GatewayOriginal),
    SdkParent(Signed<Invocation>),
}

#[derive(Clone, Debug)]
pub(super) struct OriginalGatewayTransport {
    pub(super) header: Signed<GatewayForwardHeader>,
    pub(super) original: AcceptedGatewayOriginal,
}

impl AcceptedGatewayOriginal {
    pub(super) fn request(&self) -> &Invocation {
        match self {
            Self::Full(original) => original.request(),
            Self::SdkParent(invocation) => &invocation.payload,
        }
    }
    pub(super) fn commitment(&self) -> Result<ObjectHash> {
        match self {
            Self::Full(original) => original.hash(),
            Self::SdkParent(invocation) => gateway_forward::gateway_invocation_original_hash(
                gateway_forward::GatewayForwardPurpose::Invoke,
                invocation,
            ),
        }
    }
}

#[derive(Clone, Debug)]
pub(super) enum RequestOrigin {
    Existing(TransportEvidence),
    Gateway(OriginalGatewayTransport),
    PortableIntent,
}

#[derive(Clone, Debug)]
pub(super) enum AdmissionOrigin {
    Existing(TransportEvidence),
    Gateway(OriginalGatewayTransport),
}

impl VerifiedAdmission {
    pub fn proof_usage(&self) -> &ExecutionProofUsage {
        &self.usage
    }
    pub fn admission_expires_at_ms(&self) -> u64 {
        self.admission_expires_at_ms
    }

    pub(super) fn from_execution_checkpoint(
        invocation: Signed<Invocation>,
        admitted_at_ms: u64,
        transport: TransportEvidence,
        usage: ExecutionProofUsage,
        admission_expires_at_ms: u64,
    ) -> Result<Self> {
        require(
            admission_expires_at_ms == 0 || admitted_at_ms < admission_expires_at_ms,
            "execution checkpoint has no valid admission deadline",
        )?;
        usage.enforce_limits()?;
        Ok(Self {
            invocation,
            admitted_at_ms,
            origin: AdmissionOrigin::Existing(transport),
            usage,
            admission_expires_at_ms,
            destination: None,
        })
    }
}

impl VerifiedPolicySet {
    pub fn verify_original_admission(
        &self,
        proof: &AdmissionProof,
        facts: &RequestFacts,
    ) -> Result<VerifiedAdmission> {
        let _scope = ScopedSignatureVerification::new();
        require(
            proof.challenge.stage == CheckStage::Admit
                && proof.challenge.policies.len() <= MAX_POLICY_DOMAINS
                && proof
                    .challenge
                    .policies
                    .windows(2)
                    .all(|pair| pair[0].domain < pair[1].domain)
                && proof.receipts.len() <= MAX_POLICY_DOMAINS
                && proof.invocation.payload.body_hash == ObjectHash::body(&proof.source_body)?
                && facts.body_hash == proof.invocation.payload.body_hash,
            "original admission changed its complete source body or stage",
        )?;
        let historical = remote_resolution_history::at_heads(self, &proof.challenge.policies)?;
        let original;
        let existing;
        let transport = match &proof.transport {
            StoredTransport::DirectedGateway { header } => {
                original = gateway_invocation_original(&proof.invocation, &proof.source_body);
                InvocationTransport::GatewayTranscript {
                    header,
                    original: &original,
                }
            }
            _ => {
                existing = proof.transport.existing_evidence()?;
                InvocationTransport::Existing(&existing)
            }
        };
        let request = historical.verify_original_request_for_body(
            &proof.invocation,
            &proof.source_body,
            facts,
            transport,
            proof.admitted_at_ms,
            AuthorizationPhase::Admission,
        )?;
        let fresh = historical.verify_freshness(
            &request,
            &proof.challenge,
            &proof.receipts,
            proof.admitted_at_ms,
        )?;
        historical.admit_operation(&request, &fresh, proof.admitted_at_ms)
    }

    pub fn verify_admitted_operation_authorization(
        &self,
        admission: &VerifiedAdmission,
        facts: &RequestFacts,
        now_ms: u64,
    ) -> Result<VerifiedAuthorization> {
        let _scope = ScopedSignatureVerification::new();
        require(
            now_ms >= admission.admitted_at_ms,
            "operation clock moved backwards",
        )?;
        match &admission.origin {
            AdmissionOrigin::Existing(transport) => {
                if let Some(retained) = &admission.destination {
                    let destination = self.verify_execution_destination(
                        &facts.resource,
                        &facts.audience,
                        &retained.resource,
                        &retained.public_key,
                        &retained.grant_leaf,
                        &facts.context,
                        facts.action,
                        now_ms,
                    )?;
                    self.verify_invocation_with_execution_transport(
                        &admission.invocation,
                        facts,
                        InvocationTransport::Existing(transport),
                        &destination,
                        now_ms,
                        AuthorizationPhase::Operation,
                    )
                } else {
                    self.verify_invocation_inner(
                        &admission.invocation,
                        facts,
                        transport,
                        now_ms,
                        AuthorizationPhase::Operation,
                    )
                }
            }
            AdmissionOrigin::Gateway(original) => match &original.original {
                AcceptedGatewayOriginal::Full(full) => self.verify_original_request_for_body(
                    &admission.invocation,
                    full.body(),
                    facts,
                    InvocationTransport::OriginalGateway(original),
                    now_ms,
                    AuthorizationPhase::Operation,
                ),
                AcceptedGatewayOriginal::SdkParent(_) => {
                    let mut budget = Budget::default();
                    let hash = budget.verify(&admission.invocation)?;
                    self.verify_authorized_invocation_body(
                        &admission.invocation,
                        hash,
                        budget,
                        facts,
                        InvocationTransport::OriginalGateway(original),
                        now_ms,
                        AuthorizationPhase::Operation,
                    )
                }
            },
        }
    }

    pub(super) fn verify_compact_sdk_parent_admission(
        &self,
        source: &execution_handoff::VerifiedProjectExecutionSource,
        handoff: &execution_handoff::ExecutionHandoff,
        now_ms: u64,
    ) -> Result<Option<VerifiedAdmission>> {
        let _scope = ScopedSignatureVerification::new();
        require(
            handoff.source_statement()?.capsule_hash == *source.capsule_hash(),
            "SDK parent belongs to another verified Project source",
        )?;
        let Some(origin) = &handoff.sdk_origin else {
            return Ok(None);
        };
        let event: events::Event =
            serde_json::from_str(&handoff.project_dispatch.canonical_body)
                .map_err(|_| AuthorizationError::InvalidProof("invalid SDK Project dispatch"))?;
        let events::EventCause::Invocation { admission: child } = &event.cause else {
            return Err(AuthorizationError::InvalidProof(
                "SDK dispatch has no admitted invocation",
            ));
        };
        let parent = &origin.invocation.payload;
        let Actor::Service(service) = &child.invocation.payload.actor else {
            return Err(AuthorizationError::InvalidProof(
                "SDK parent has no Service child",
            ));
        };
        let session = child
            .invocation
            .payload
            .context
            .sdk_session
            .as_ref()
            .ok_or(AuthorizationError::InvalidProof(
                "SDK child has no adapter session",
            ))?;
        require(
            matches!(parent.actor, Actor::Account(_))
                && parent.initiator == parent.actor
                && parent.initiating_grant.is_none()
                && parent.action == Action::MessageAppend
                && parent.context.sdk_session.is_none()
                && parent.context.execution_session.is_none()
                && parent.resource == child.invocation.payload.resource
                && parent.audience == child.invocation.payload.audience
                && parent.context.conversation == child.invocation.payload.context.conversation
                && parent.context.workspace_resource
                    == child.invocation.payload.context.workspace_resource
                && session.target == child.invocation.payload.device
                && session.session_id == origin.sdk_trigger.as_str()
                && child.source_body["parent_invocation"].as_str()
                    == Some(origin.invocation.hash()?.as_str())
                && child.source_body["sdk_session"]
                    == serde_json::to_value(session)
                        .map_err(|_| AuthorizationError::InvalidProof("invalid SDK session"))?
                && child.transport.existing_evidence()?
                    == TransportEvidence::LocalAgentAdapter {
                        service: service.clone(),
                        session: session.clone(),
                    }
                && origin.admitted_at_ms <= child.admitted_at_ms
                && child.admitted_at_ms <= now_ms
                && origin.challenge.stage == CheckStage::Admit
                && origin.challenge.policies.len() <= MAX_POLICY_DOMAINS
                && origin
                    .challenge
                    .policies
                    .windows(2)
                    .all(|pair| pair[0].domain < pair[1].domain)
                && origin.receipts.len() <= MAX_POLICY_DOMAINS,
            "compact SDK parent changed its verified Account origin",
        )?;
        let historical = remote_resolution_history::at_heads(self, &origin.challenge.policies)?;
        let facts = RequestFacts {
            resource: parent.resource.clone(),
            action: parent.action,
            audience: parent.audience.clone(),
            operation_id: parent.operation_id.clone(),
            body_hash: parent.body_hash.clone(),
            context: parent.context.clone(),
        };
        let existing;
        let transport = match &origin.transport {
            StoredTransport::DirectedGateway { header } => InvocationTransport::CompactSdkParent {
                header,
                invocation: &origin.invocation,
            },
            _ => {
                existing = origin.transport.existing_evidence()?;
                InvocationTransport::Existing(&existing)
            }
        };
        let mut budget = Budget::default();
        let hash = budget.verify(&origin.invocation)?;
        let authorization = historical.verify_authorized_invocation_body(
            &origin.invocation,
            hash,
            budget,
            &facts,
            transport,
            origin.admitted_at_ms,
            AuthorizationPhase::Admission,
        )?;
        let freshness = historical.verify_freshness(
            &authorization,
            &origin.challenge,
            &origin.receipts,
            origin.admitted_at_ms,
        )?;
        let mut admission =
            historical.admit_operation(&authorization, &freshness, origin.admitted_at_ms)?;
        admission.usage.merge(source.proof_usage());
        if let Some(counts) = SignatureVerification::current_counts() {
            admission.usage.record_actual_signature_counts(counts);
        }
        admission.usage.enforce_limits()?;
        Ok(Some(admission))
    }

    fn verify_original_request_for_body(
        &self,
        signed: &Signed<Invocation>,
        body: &serde_json::Value,
        facts: &RequestFacts,
        transport: InvocationTransport<'_>,
        now_ms: u64,
        phase: AuthorizationPhase,
    ) -> Result<VerifiedAuthorization> {
        require(
            ObjectHash::body(body)? == facts.body_hash,
            "admission body differs from request facts",
        )?;
        if let Some((resource, key, grant)) = prepare::remote_selection(body)? {
            let destination = self.verify_execution_destination(
                &facts.resource,
                &facts.audience,
                &resource,
                &key,
                &grant,
                &facts.context,
                facts.action,
                now_ms,
            )?;
            self.verify_invocation_with_execution_transport(
                signed,
                facts,
                transport,
                &destination,
                now_ms,
                phase,
            )
        } else {
            let mut budget = Budget::default();
            let hash = budget.verify(signed)?;
            self.verify_authorized_invocation_body(
                signed, hash, budget, facts, transport, now_ms, phase,
            )
        }
    }
}

pub(super) fn admission_usage(
    auth: &VerifiedAuthorization,
    fresh: &VerifiedFreshness,
) -> Result<ExecutionProofUsage> {
    let mut usage = fresh.usage.clone();
    usage.record_authorization(auth)?;
    if let Some(counts) = SignatureVerification::current_counts() {
        usage.record_actual_signature_counts(counts);
    }
    usage.enforce_limits()?;
    Ok(usage)
}

pub(super) fn verify_compact_gateway_parent(
    header: &Signed<GatewayForwardHeader>,
    invocation: &Signed<Invocation>,
    target: &PublicKey,
    now_ms: u64,
) -> Result<()> {
    use gateway_forward::GatewayForwardPurpose;
    header.verify_signature()?;
    invocation.verify_signature()?;
    let h = &header.payload;
    let i = &invocation.payload;
    validate_window(h.issued_at_ms, h.expires_at_ms, now_ms)?;
    validate_window(i.not_before_ms, i.expires_at_ms, now_ms)?;
    require(
        h.version == AUTH_VERSION
            && i.version == AUTH_VERSION
            && h.purpose == GatewayForwardPurpose::Invoke
            && i.action == Action::MessageAppend
            && matches!(i.actor, Actor::Account(_))
            && i.initiator == i.actor
            && i.initiating_grant.is_none()
            && i.context.sdk_session.is_none()
            && i.context.execution_session.is_none()
            && header.issuer == h.gateway_key
            && h.client_key == invocation.issuer
            && h.client_key != h.gateway_key
            && h.target_key == *target
            && h.target_key != h.gateway_key
            && i.device == DeviceId::from_key(&invocation.issuer)
            && i.presenter == DeviceId::from_key(&h.gateway_key)
            && i.audience.device == DeviceId::from_key(target)
            && valid_operation_id(&i.operation_id)
            && h.authorization_id == i.operation_id
            && h.lookup_operation_id.is_none()
            && h.original_hash
                == gateway_forward::gateway_invocation_original_hash(
                    GatewayForwardPurpose::Invoke,
                    invocation,
                )?
            && h.issued_at_ms >= i.not_before_ms
            && h.expires_at_ms <= i.expires_at_ms,
        "compact SDK gateway parent changed signed origin or window",
    )
}
