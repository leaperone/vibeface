//! Read-only receipts for the original typed Task business challenges.

use super::super::{
    execution_handoff::{ExecutionHandoffChallenge, ExecutionHandoffResolutionChallenge},
    execution_observation::ExecutionObservationChallenge,
};
use super::*;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(
    tag = "type",
    content = "challenge",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum TaskBusinessChallenge {
    Submit(ExecutionHandoffChallenge),
    Observe(ExecutionObservationChallenge),
    Resolve(ExecutionHandoffResolutionChallenge),
}

impl TaskBusinessChallenge {
    pub fn hash(&self) -> Result<ObjectHash> {
        match self {
            Self::Submit(value) => value.hash(),
            Self::Observe(value) => value.hash(),
            Self::Resolve(value) => value.hash(),
        }
    }
    pub fn policies(&self) -> &[PolicyStamp] {
        match self {
            Self::Submit(value) => &value.policies,
            Self::Observe(value) => &value.policies,
            Self::Resolve(value) => &value.policies,
        }
    }
    pub fn issued_at_ms(&self) -> u64 {
        match self {
            Self::Submit(value) => value.issued_at_ms,
            Self::Observe(value) => value.issued_at_ms,
            Self::Resolve(value) => value.issued_at_ms,
        }
    }
    pub fn expires_at_ms(&self) -> u64 {
        match self {
            Self::Submit(value) => value.expires_at_ms,
            Self::Observe(value) => value.expires_at_ms,
            Self::Resolve(value) => value.expires_at_ms,
        }
    }

    fn verify_current(
        &self,
        current: &VerifiedTaskCurrentPolicy,
        authorization: TaskBusinessAuthorization<'_>,
        wire: &TaskBusinessWire,
        now: u64,
        usage: &mut ExecutionProofUsage,
    ) -> Result<u64> {
        validate_window(self.issued_at_ms(), self.expires_at_ms(), now)?;
        require(
            self.policies().len() <= MAX_POLICY_DOMAINS
                && self
                    .policies()
                    .windows(2)
                    .all(|pair| pair[0].domain < pair[1].domain),
            "Task business challenge has invalid exact policy heads",
        )?;
        let (heads, expires, expected) = match (self, authorization, wire) {
            (
                Self::Submit(challenge),
                TaskBusinessAuthorization::Submit(value),
                TaskBusinessWire::Submit(original),
            ) => {
                require(
                    value.wire() == original,
                    "Task business receipt changed its verified submission",
                )?;
                current
                    .state()
                    .check_execution_handoff_current(value, now)?;
                let expected = ExecutionHandoffChallenge::bound_to(
                    value,
                    challenge.stage,
                    challenge.nonce.clone(),
                    challenge.issued_at_ms,
                    challenge.expires_at_ms,
                )?;
                usage.merge(value.proof_usage());
                (
                    value.dependencies(),
                    value.expires_at_ms(),
                    Self::Submit(expected),
                )
            }
            (
                Self::Observe(challenge),
                TaskBusinessAuthorization::Observe(value),
                TaskBusinessWire::Observe(original),
            ) => {
                require(
                    value.hash() == &original.hash()?,
                    "Task business receipt changed its verified observation",
                )?;
                current
                    .state()
                    .check_execution_observation_current(value, now)?;
                let expected = ExecutionObservationChallenge::bound_to(
                    value,
                    challenge.nonce.clone(),
                    challenge.issued_at_ms,
                    challenge.expires_at_ms,
                )?;
                usage.merge(value.proof_usage());
                (
                    value.heads(),
                    value.expires_at_ms(),
                    Self::Observe(expected),
                )
            }
            (
                Self::Resolve(challenge),
                TaskBusinessAuthorization::Resolve(value),
                TaskBusinessWire::Resolve(original),
            ) => {
                require(
                    value.wire() == original,
                    "Task business receipt changed its verified resolution",
                )?;
                current.state().check_current(value.resolver(), now)?;
                current.state().check_current(value.submission(), now)?;
                let expected = ExecutionHandoffResolutionChallenge::bound_to(
                    value,
                    challenge.nonce.clone(),
                    challenge.issued_at_ms,
                    challenge.expires_at_ms,
                )?;
                usage.merge(value.proof_usage());
                (
                    value.dependencies(),
                    value.expires_at_ms(),
                    Self::Resolve(expected),
                )
            }
            _ => {
                return Err(AuthorizationError::InvalidProof(
                    "Task business receipt changed its typed challenge purpose",
                ));
            }
        };
        if self.policies().iter().any(|old| {
            heads
                .iter()
                .any(|head| head.domain == old.domain && head != old)
        }) {
            return Err(AuthorizationError::NeedPolicySync(heads.to_vec()));
        }
        require(
            self == &expected,
            "Task business receipt changed a complete challenge anchor",
        )?;
        Ok(expires.min(self.expires_at_ms()))
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct TaskBusinessReceiptRead {
    pub version: u16,
    pub source_header_hash: ObjectHash,
    pub business_wire_hash: ObjectHash,
    pub challenge: TaskBusinessChallenge,
    pub nonce: Nonce,
    pub issued_at_ms: u64,
    pub expires_at_ms: u64,
}

impl sealed::Sealed for TaskBusinessReceiptRead {}
impl SignedObject for TaskBusinessReceiptRead {
    const DOMAIN: &'static str = "task-business-receipt-read-v2";
}

#[derive(Clone)]
pub struct VerifiedTaskBusinessReceiptAuthority {
    scope: TaskBusinessScope,
    challenge: TaskBusinessChallenge,
    challenge_hash: ObjectHash,
    primary_heads: Vec<PolicyStamp>,
    signing_heads: Vec<PolicyStamp>,
    local_b: PublicKey,
    verified_at_ms: u64,
    expires_at_ms: u64,
    usage: ExecutionProofUsage,
}

pub fn authorize_task_business_challenge(
    primary: &VerifiedPolicySet,
    current: &VerifiedTaskCurrentPolicy,
    authorization: TaskBusinessAuthorization<'_>,
    wire: &TaskBusinessWire,
    challenge: &TaskBusinessChallenge,
    local_b: &PublicKey,
    now: u64,
) -> Result<VerifiedTaskBusinessReceiptAuthority> {
    let _signatures = ScopedSignatureVerification::new();
    carrier(challenge)?;
    let mut usage = ExecutionProofUsage::default();
    usage.record_metadata(challenge)?;
    authorize_inner(
        primary,
        current,
        authorization,
        wire,
        challenge,
        local_b,
        now,
        usage,
    )
}

pub fn verify_task_business_receipt_read(
    primary: &VerifiedPolicySet,
    current: &VerifiedTaskCurrentPolicy,
    authorization: TaskBusinessAuthorization<'_>,
    original_header: &Signed<TaskBusinessPolicyHeader>,
    original_wire: &TaskBusinessWire,
    actual_c_noise: &ActualPolicyNoisePeer,
    request: &Signed<TaskBusinessReceiptRead>,
    local_b: &PublicKey,
    now: u64,
) -> Result<VerifiedTaskBusinessReceiptAuthority> {
    let _signatures = ScopedSignatureVerification::new();
    carrier(request)?;
    carrier(original_header)?;
    if let TaskBusinessChallenge::Submit(challenge) = &request.payload.challenge {
        require(
            matches!(
                challenge.stage,
                CheckStage::Start | CheckStage::Renew | CheckStage::RemoteRead
            ),
            "Task business admission receipts require their original Client session",
        )?;
    }
    window(
        request.payload.issued_at_ms,
        request.payload.expires_at_ms,
        now,
    )?;
    require(
        request.payload.version == AUTH_VERSION
            && original_header.payload.version == AUTH_VERSION
            && &original_header.issuer == local_b
            && &original_header.payload.scope.source_node_public_key == local_b
            && request.payload.source_header_hash == original_header.hash()?
            && request.payload.business_wire_hash
                == original_header.payload.scope.business_wire_hash
            && original_wire.scope()? == original_header.payload.scope
            && request.issuer == original_header.payload.scope.target_public_key
            && &request.issuer == actual_c_noise.public_key()
            && request.payload.issued_at_ms >= request.payload.challenge.issued_at_ms()
            && request.payload.expires_at_ms <= request.payload.challenge.expires_at_ms(),
        "Task business receipt read changed its actual executor, original wire or challenge",
    )?;
    original_header.verify_signature()?;
    request.verify_signature()?;
    let mut usage = ExecutionProofUsage::default();
    usage.record_signed(original_header)?;
    usage.record_signed(request)?;
    let mut verified = authorize_inner(
        primary,
        current,
        authorization,
        original_wire,
        &request.payload.challenge,
        local_b,
        now,
        usage,
    )?;
    verified.expires_at_ms = verified.expires_at_ms.min(request.payload.expires_at_ms);
    Ok(verified)
}

fn authorize_inner(
    primary: &VerifiedPolicySet,
    current: &VerifiedTaskCurrentPolicy,
    authorization: TaskBusinessAuthorization<'_>,
    wire: &TaskBusinessWire,
    challenge: &TaskBusinessChallenge,
    local_b: &PublicKey,
    now: u64,
    mut usage: ExecutionProofUsage,
) -> Result<VerifiedTaskBusinessReceiptAuthority> {
    let scope = wire.scope()?;
    require(
        &scope.source_node_public_key == local_b,
        "Task business receipt requires the original source Node",
    )?;
    check_source_policy_scope(current, wire, &scope, now)?;
    let expires = challenge
        .verify_current(current, authorization, wire, now, &mut usage)?
        .min(current.expires_at_ms());
    require(
        now < expires,
        "Task business receipt current read authority expired",
    )?;
    let mut primary_heads = Vec::new();
    for head in current.primary_heads() {
        if primary.head(&head.domain) != Some(head) {
            return Err(AuthorizationError::NeedPolicySync(
                current.primary_heads().to_vec(),
            ));
        }
        primary_heads.push(head.clone());
    }
    let mut signing_heads = Vec::new();
    for head in challenge.policies() {
        if !primary_heads
            .iter()
            .any(|current| current.domain == head.domain)
        {
            continue;
        }
        if primary.head(&head.domain) != Some(head) {
            return Err(AuthorizationError::NeedPolicySync(primary_heads));
        }
        let authority = match &head.domain {
            PolicyDomain::Account(account) => {
                primary.account(account).map(|value| &value.authority)
            }
            PolicyDomain::Space(space) => primary.space(space).map(|value| &value.authority),
        }
        .ok_or(AuthorizationError::PolicyConflict)?;
        if &authority.public_key == local_b {
            require(
                authority.device == DeviceId::from_key(local_b)
                    && authority.service == ServiceId::from_key(local_b),
                "Task business receipt authority changed its service or device",
            )?;
            signing_heads.push(head.clone());
        }
    }
    finish_usage(&mut usage)?;
    Ok(VerifiedTaskBusinessReceiptAuthority {
        scope,
        challenge: challenge.clone(),
        challenge_hash: challenge.hash()?,
        primary_heads,
        signing_heads,
        local_b: local_b.clone(),
        verified_at_ms: now,
        expires_at_ms: expires,
        usage,
    })
}

impl VerifiedTaskBusinessReceiptAuthority {
    pub fn scope(&self) -> &TaskBusinessScope {
        &self.scope
    }
    pub fn challenge(&self) -> &TaskBusinessChallenge {
        &self.challenge
    }
    pub fn challenge_hash(&self) -> &ObjectHash {
        &self.challenge_hash
    }
    pub fn primary_heads(&self) -> &[PolicyStamp] {
        &self.primary_heads
    }
    pub fn signing_heads(&self) -> &[PolicyStamp] {
        &self.signing_heads
    }
    pub fn expires_at_ms(&self) -> u64 {
        self.expires_at_ms
    }
    pub fn proof_usage(&self) -> &ExecutionProofUsage {
        &self.usage
    }

    pub fn sign_receipts(
        &self,
        identity: &Identity,
        now: u64,
    ) -> Result<Vec<Signed<FreshnessReceipt>>> {
        let _signatures = ScopedSignatureVerification::new();
        require(
            PublicKey::of(identity) == self.local_b
                && now == self.verified_at_ms
                && now < self.expires_at_ms,
            "Task business receipts require their freshly verified source authority",
        )?;
        let receipts = self
            .signing_heads
            .iter()
            .map(|head| {
                Signed::sign(
                    FreshnessReceipt {
                        version: AUTH_VERSION,
                        current: head.clone(),
                        challenge_hash: self.challenge_hash.clone(),
                        issued_at_ms: now,
                        expires_at_ms: self.expires_at_ms,
                    },
                    identity,
                )
            })
            .collect::<Result<Vec<_>>>()?;
        carrier(&receipts)?;
        let mut usage = self.usage.clone();
        for receipt in &receipts {
            usage.record_signed(receipt)?;
        }
        finish_usage(&mut usage)?;
        Ok(receipts)
    }
}
