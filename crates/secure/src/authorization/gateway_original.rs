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
pub(super) struct OriginalGatewayTransport {
    pub(super) header: Signed<GatewayForwardHeader>,
    pub(super) original: GatewayOriginal,
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
            AdmissionOrigin::Gateway(original) => self.verify_original_request_for_body(
                &admission.invocation,
                original.original.body(),
                facts,
                InvocationTransport::OriginalGateway(original),
                now_ms,
                AuthorizationPhase::Operation,
            ),
        }
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
