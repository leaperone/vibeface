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
