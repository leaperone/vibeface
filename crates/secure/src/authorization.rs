//! Typed, root-approved policy and single-path capability verification.

pub mod authenticators;
pub mod bootstrap;
pub mod directory;
pub mod projection;
pub mod services;
use crate::{Identity, membership::canonical_json};
pub use authenticators::{AuthenticatorRecord, PasskeyClientEnrollment, VerifiedAuthenticator};
use serde::{Deserialize, Deserializer, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

pub const AUTH_VERSION: u16 = 2;
pub const MAX_DELEGATION_DEPTH: usize = 8;
pub const MAX_PROOF_OBJECTS: usize = 32;
pub const MAX_PROOF_SIGNATURES: usize = 32;
pub const MAX_PROOF_BYTES: usize = 65_536;
pub const MAX_POLICY_DOMAINS: usize = 10;
pub const MAX_POLICY_HISTORY: usize = 1024;
pub const INVOCATION_MAX_AGE_MS: u64 = 120_000;
pub const REMOTE_READ_MAX_AGE_MS: u64 = 600_000;
pub const EXECUTION_LEASE_MS: u64 = 30_000;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AuthorizationError {
    InvalidProof(&'static str),
    Revoked,
    Expired,
    BudgetExceeded,
    PolicyConflict,
    NeedPolicySync(Vec<PolicyStamp>),
    NeedFreshness(Vec<PolicyDomain>),
    UserPresenceRequired,
}
impl fmt::Display for AuthorizationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{self:?}")
    }
}
impl std::error::Error for AuthorizationError {}
pub type Result<T> = std::result::Result<T, AuthorizationError>;

fn require(condition: bool, reason: &'static str) -> Result<()> {
    if condition {
        Ok(())
    } else {
        Err(AuthorizationError::InvalidProof(reason))
    }
}
fn lower_hex(value: &str, bytes: usize) -> bool {
    value.len() == bytes * 2
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
macro_rules! text_type {
    ($name:ident, $prefix:literal, $bytes:expr) => {
        #[derive(Clone, Debug, Serialize, PartialEq, Eq, PartialOrd, Ord, Hash)]
        #[serde(transparent)]
        pub struct $name(String);
        impl $name {
            pub fn parse(value: &str) -> Result<Self> {
                let suffix = value
                    .strip_prefix($prefix)
                    .ok_or(AuthorizationError::InvalidProof("invalid typed identifier"))?;
                require(
                    lower_hex(suffix, $bytes),
                    "invalid lowercase hexadecimal value",
                )?;
                Ok(Self(value.to_owned()))
            }
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }
        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D: Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
                Self::parse(&String::deserialize(d)?).map_err(serde::de::Error::custom)
            }
        }
        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }
    };
}
text_type!(AccountId, "acc2_", 32);
text_type!(DeviceId, "dev2_", 32);
text_type!(SpaceId, "spc2_", 32);
text_type!(ResourceId, "res2_", 32);
text_type!(ServiceId, "svc2_", 32);
text_type!(ObjectHash, "h2_", 32);
text_type!(PublicKey, "", 32);
text_type!(Signature, "", 64);
text_type!(Nonce, "", 32);
fn digest(domain: &str, bytes: &[u8]) -> String {
    let mut hash = Sha256::new();
    hash.update(domain.as_bytes());
    hash.update([0]);
    hash.update(bytes);
    hex::encode(hash.finalize())
}
impl PublicKey {
    pub fn of(identity: &Identity) -> Self {
        Self(identity.public_key_hex())
    }
}
impl AccountId {
    pub fn from_root(key: &PublicKey) -> Self {
        Self(format!(
            "acc2_{}",
            digest("account-v2", key.as_str().as_bytes())
        ))
    }
}
impl DeviceId {
    pub fn from_key(key: &PublicKey) -> Self {
        Self(format!(
            "dev2_{}",
            digest("device-v2", key.as_str().as_bytes())
        ))
    }
}
impl ServiceId {
    pub fn from_key(key: &PublicKey) -> Self {
        Self(format!(
            "svc2_{}",
            digest("service-v2", key.as_str().as_bytes())
        ))
    }
}
impl SpaceId {
    pub fn personal(owner: &AccountId) -> Self {
        Self(format!(
            "spc2_{}",
            digest("personal-space-v2", owner.as_str().as_bytes())
        ))
    }
    pub fn team(root: &PublicKey) -> Self {
        Self(format!(
            "spc2_{}",
            digest("team-space-v2", root.as_str().as_bytes())
        ))
    }
}
impl ResourceId {
    pub fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(format!("res2_{}", hex::encode(bytes)))
    }
    pub fn random() -> Result<Self> {
        let mut bytes = [0; 32];
        getrandom::fill(&mut bytes)
            .map_err(|_| AuthorizationError::InvalidProof("randomness unavailable"))?;
        Ok(Self::from_bytes(bytes))
    }
}
impl Nonce {
    pub fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(hex::encode(bytes))
    }
    pub fn random() -> Result<Self> {
        let mut bytes = [0; 32];
        getrandom::fill(&mut bytes)
            .map_err(|_| AuthorizationError::InvalidProof("randomness unavailable"))?;
        Ok(Self::from_bytes(bytes))
    }
}
impl ObjectHash {
    pub fn body<T: Serialize>(body: &T) -> Result<Self> {
        Ok(Self(format!(
            "h2_{}",
            digest("request-body-v2", &canonical(body)?)
        )))
    }
}
fn canonical<T: Serialize>(value: &T) -> Result<Vec<u8>> {
    canonical_json(value).map_err(|_| AuthorizationError::InvalidProof("invalid canonical JSON"))
}
mod sealed {
    pub trait Sealed {}
}
pub trait SignedObject: Serialize + sealed::Sealed {
    const DOMAIN: &'static str;
}
macro_rules! signed_object {
    ($name:ident, $domain:literal) => {
        impl sealed::Sealed for $name {}
        impl SignedObject for $name {
            const DOMAIN: &'static str = $domain;
        }
    };
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Signed<T> {
    pub payload: T,
    pub issuer: PublicKey,
    pub signature: Signature,
}
#[derive(Serialize)]
struct Unsigned<'a, T> {
    issuer: &'a PublicKey,
    payload: &'a T,
}
impl<T: SignedObject> Signed<T> {
    pub fn sign(payload: T, identity: &Identity) -> Result<Self> {
        let issuer = PublicKey::of(identity);
        let bytes = canonical(&Unsigned {
            issuer: &issuer,
            payload: &payload,
        })?;
        let signature = Signature::parse(
            &identity
                .sign(T::DOMAIN, &bytes)
                .map_err(|_| AuthorizationError::InvalidProof("signing failed"))?,
        )?;
        Ok(Self {
            payload,
            issuer,
            signature,
        })
    }
    pub fn signing_bytes(&self) -> Result<Vec<u8>> {
        canonical(&Unsigned {
            issuer: &self.issuer,
            payload: &self.payload,
        })
    }
    pub fn hash(&self) -> Result<ObjectHash> {
        Ok(ObjectHash(format!(
            "h2_{}",
            digest(T::DOMAIN, &self.signing_bytes()?)
        )))
    }
    fn verify_with_budget(&self, budget: &mut Budget) -> Result<()> {
        budget.verify_raw(
            &self.issuer,
            T::DOMAIN,
            &self.signing_bytes()?,
            &self.signature,
        )
    }
    pub fn verify_signature(&self) -> Result<()> {
        self.verify_with_budget(&mut Budget::default())
    }
}
pub fn decode_signed<T: SignedObject + for<'de> Deserialize<'de>>(
    bytes: &[u8],
) -> Result<Signed<T>> {
    require(bytes.len() <= MAX_PROOF_BYTES, "signed object too large")?;
    let value: Signed<T> = serde_json::from_slice(bytes)
        .map_err(|_| AuthorizationError::InvalidProof("invalid typed JSON"))?;
    canonical(&value)?;
    Ok(value)
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(
    tag = "kind",
    content = "id",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum Actor {
    Account(AccountId),
    Service(ServiceId),
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(
    tag = "kind",
    content = "id",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum PolicyDomain {
    Account(AccountId),
    Space(SpaceId),
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(deny_unknown_fields)]
pub struct PolicyStamp {
    pub domain: PolicyDomain,
    pub authority_epoch: u64,
    pub sequence: u64,
    pub head: ObjectHash,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct AuthorityBinding {
    pub service: ServiceId,
    pub device: DeviceId,
    pub public_key: PublicKey,
}
impl AuthorityBinding {
    pub fn from_key(key: PublicKey) -> Self {
        Self {
            service: ServiceId::from_key(&key),
            device: DeviceId::from_key(&key),
            public_key: key,
        }
    }
    fn validate(&self, root: &PublicKey) -> Result<()> {
        require(
            self.device == DeviceId::from_key(&self.public_key)
                && self.service == ServiceId::from_key(&self.public_key)
                && &self.public_key != root,
            "invalid policy authority",
        )
    }
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RecoveryPolicy {
    pub generation: u64,
    pub keys: Vec<PublicKey>,
    pub threshold: u8,
}
impl RecoveryPolicy {
    fn validate(&self, root: &PublicKey) -> Result<()> {
        require(
            !self.keys.is_empty()
                && self.keys.len() <= 8
                && sorted_unique(&self.keys)
                && self.threshold > 0
                && usize::from(self.threshold) <= self.keys.len()
                && !self.keys.contains(root),
            "invalid independent recovery policy",
        )
    }
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct KeyApproval {
    pub key: PublicKey,
    pub signature: Signature,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum AccountRight {
    EnrollDevice,
    RevokeDevice,
    ManageAuthenticators,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct EnrollmentAuthority {
    pub device: DeviceId,
    pub rights: Vec<AccountRight>,
    pub expires_at_ms: u64,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct AccountDocument {
    pub version: u16,
    pub account: AccountId,
    pub genesis_root: PublicKey,
    pub current_root: PublicKey,
    pub root_generation: u64,
    pub previous: Option<PolicyStamp>,
    pub authority_epoch: u64,
    pub sequence: u64,
    pub authority: AuthorityBinding,
    pub recovery: RecoveryPolicy,
    pub enrollment: Vec<EnrollmentAuthority>,
    pub devices: Vec<ObjectHash>,
    pub revoked_devices: Vec<DeviceId>,
    pub authenticators: Vec<ObjectHash>,
    pub root_acceptance: Option<Signature>,
    pub recovery_approvals: Vec<KeyApproval>,
}
signed_object!(AccountDocument, "account-document-v2");
impl AccountDocument {
    pub fn governance_bytes(&self) -> Result<Vec<u8>> {
        let mut document = self.clone();
        document.root_acceptance = None;
        document.recovery_approvals.clear();
        canonical(&document)
    }
    pub fn accept_root(&mut self, next_root: &Identity) -> Result<()> {
        self.root_acceptance = Some(Signature::parse(
            &next_root
                .sign("account-root-accept-v2", &self.governance_bytes()?)
                .map_err(|_| AuthorizationError::InvalidProof("root acceptance signing failed"))?,
        )?);
        Ok(())
    }
    pub fn approve_recovery(&mut self, recovery: &Identity) -> Result<()> {
        let key = PublicKey::of(recovery);
        let signature = Signature::parse(
            &recovery
                .sign("account-recovery-v2", &self.governance_bytes()?)
                .map_err(|_| AuthorizationError::InvalidProof("recovery signing failed"))?,
        )?;
        self.recovery_approvals.push(KeyApproval { key, signature });
        Ok(())
    }
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum SpaceRoot {
    Personal {
        owner: AccountId,
        genesis_root: PublicKey,
    },
    Team {
        genesis_root: PublicKey,
    },
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ResourceRef {
    pub space: SpaceId,
    pub resource: ResourceId,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ResourceKind {
    SpaceControl,
    Project,
    ExecutionService,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ResourceRecord {
    pub version: u16,
    pub space: SpaceId,
    pub resource: ResourceId,
    pub kind: ResourceKind,
    pub host: ServiceId,
    pub issued_under: PolicyStamp,
    pub nonce: Nonce,
}
signed_object!(ResourceRecord, "resource-record-v2");
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SpaceDocument {
    pub version: u16,
    pub space: SpaceId,
    pub root: SpaceRoot,
    pub current_root: PublicKey,
    pub root_generation: u64,
    pub previous: Option<PolicyStamp>,
    pub authority_epoch: u64,
    pub sequence: u64,
    pub authority: AuthorityBinding,
    pub recovery: RecoveryPolicy,
    pub devices: Vec<ObjectHash>,
    pub revoked_devices: Vec<DeviceId>,
    pub memberships: Vec<ObjectHash>,
    pub grants: Vec<ObjectHash>,
    pub resources: Vec<ObjectHash>,
    pub governance_grant: Option<ObjectHash>,
    pub root_acceptance: Option<Signature>,
    pub recovery_approvals: Vec<KeyApproval>,
}
signed_object!(SpaceDocument, "space-document-v2");
impl SpaceDocument {
    pub fn governance_bytes(&self) -> Result<Vec<u8>> {
        let mut document = self.clone();
        document.root_acceptance = None;
        document.recovery_approvals.clear();
        canonical(&document)
    }
    pub fn accept_root(&mut self, next_root: &Identity) -> Result<()> {
        self.root_acceptance = Some(Signature::parse(
            &next_root
                .sign("space-root-accept-v2", &self.governance_bytes()?)
                .map_err(|_| AuthorizationError::InvalidProof("root acceptance signing failed"))?,
        )?);
        Ok(())
    }
    pub fn approve_recovery(&mut self, recovery: &Identity) -> Result<()> {
        let key = PublicKey::of(recovery);
        let signature = Signature::parse(
            &recovery
                .sign("space-recovery-v2", &self.governance_bytes()?)
                .map_err(|_| AuthorizationError::InvalidProof("recovery signing failed"))?,
        )?;
        self.recovery_approvals.push(KeyApproval { key, signature });
        Ok(())
    }
}
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum DeviceUse {
    Noise,
    AccountInvocation,
    ServiceInvocation,
    EnrollDevice,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct DeviceCertificate {
    pub version: u16,
    pub owner: PolicyDomain,
    pub subject: Actor,
    pub device: DeviceId,
    pub public_key: PublicKey,
    pub uses: Vec<DeviceUse>,
    pub possession: Option<Signature>,
    pub enrollment_grant: Option<ObjectHash>,
    pub issued_under: PolicyStamp,
    pub nonce: Nonce,
    pub not_before_ms: u64,
    pub expires_at_ms: u64,
}
signed_object!(DeviceCertificate, "device-certificate-v2");
impl DeviceCertificate {
    pub fn possession_bytes(&self) -> Result<Vec<u8>> {
        let mut certificate = self.clone();
        certificate.possession = None;
        canonical(&certificate)
    }
    pub fn prove_possession(&mut self, device: &Identity) -> Result<()> {
        require(
            self.public_key == PublicKey::of(device),
            "device possession key mismatch",
        )?;
        self.possession = Some(Signature::parse(
            &device
                .sign("device-possession-v2", &self.possession_bytes()?)
                .map_err(|_| {
                    AuthorizationError::InvalidProof("device possession signing failed")
                })?,
        )?);
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum SpaceRole {
    Viewer,
    Operator,
    ResourceAdmin,
    SpaceAdmin,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Membership {
    pub version: u16,
    pub space: SpaceId,
    pub subject: Actor,
    pub roles: Vec<SpaceRole>,
    pub issued_under: PolicyStamp,
    pub not_before_ms: u64,
    pub expires_at_ms: u64,
    pub nonce: Nonce,
}
signed_object!(Membership, "membership-v2");
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(
    tag = "kind",
    content = "value",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum ResourceScope {
    WholeSpace(SpaceId),
    Exact(ResourceRef),
}
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MemberKind {
    Account,
    Service,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct MembersOf {
    pub space: SpaceId,
    pub pinned_genesis: ObjectHash,
    pub kind: MemberKind,
    pub roles_any: Vec<SpaceRole>,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(
    tag = "kind",
    content = "value",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum Grantee {
    Actor(Actor),
    MembersOf(MembersOf),
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Audience {
    pub service: ServiceId,
    pub device: DeviceId,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SessionRef {
    pub target: DeviceId,
    pub session_id: String,
}
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct InvocationContext {
    pub conversation: Option<String>,
    pub task: Option<ObjectHash>,
    pub sdk_session: Option<SessionRef>,
    pub execution_session: Option<SessionRef>,
    pub executor_kind: Option<String>,
    pub workspace_resource: Option<ResourceId>,
}
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct GrantConstraints {
    pub context: InvocationContext,
    pub require_account_actor: bool,
    pub require_webauthn_uv: bool,
}
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum Action {
    ConversationRead,
    ConversationCreate,
    MessageAppend,
    TaskCreate,
    TaskProgress,
    TaskAccept,
    TaskCancel,
    ExecutionOpen,
    ExecutionSend,
    ExecutionSteer,
    ExecutionInterrupt,
    ExecutionRespondApproval,
    ExecutionRebind,
    OperationStatusRead,
    OperationReconcile,
    OperationResolve,
    AttachmentRead,
    AttachmentWrite,
    Export,
    NotificationRead,
    NotificationAck,
    MemberManage,
    ResourceManage,
    GrantIssue,
    GrantRevoke,
    ServiceEnroll,
    ReplicaRead,
    AgentStart,
}
impl Action {
    pub fn is_read(self) -> bool {
        matches!(
            self,
            Self::ConversationRead
                | Self::OperationStatusRead
                | Self::AttachmentRead
                | Self::Export
                | Self::NotificationRead
                | Self::ReplicaRead
        )
    }
    pub fn requires_account(self) -> bool {
        matches!(
            self,
            Self::TaskAccept
                | Self::ExecutionRespondApproval
                | Self::ExecutionRebind
                | Self::OperationResolve
        )
    }
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Grant {
    pub version: u16,
    pub owner_space: SpaceId,
    pub scope: ResourceScope,
    pub grantee: Grantee,
    pub actions: Vec<Action>,
    pub audience: Audience,
    pub constraints: GrantConstraints,
    pub parent: Option<ObjectHash>,
    pub remaining_delegations: u8,
    pub issued_under: PolicyStamp,
    pub not_before_ms: u64,
    pub expires_at_ms: u64,
    pub nonce: Nonce,
}
signed_object!(Grant, "grant-v2");
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Invocation {
    pub version: u16,
    pub actor: Actor,
    pub device: DeviceId,
    pub initiator: Actor,
    pub initiating_grant: Option<ObjectHash>,
    pub resource: ResourceRef,
    pub action: Action,
    pub audience: Audience,
    pub presenter: DeviceId,
    pub operation_id: String,
    pub body_hash: ObjectHash,
    pub grant_leaf: ObjectHash,
    pub expected_policies: Vec<PolicyStamp>,
    pub context: InvocationContext,
    pub nonce: Nonce,
    pub not_before_ms: u64,
    pub expires_at_ms: u64,
}
signed_object!(Invocation, "invocation-v2");
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CheckStage {
    RemoteRead,
    Admit,
    Start,
    Renew,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct FreshnessChallenge {
    pub version: u16,
    pub stage: CheckStage,
    pub invocation_hash: ObjectHash,
    pub operation_id: String,
    pub body_hash: ObjectHash,
    pub audience: Audience,
    pub presenter: DeviceId,
    pub policies: Vec<PolicyStamp>,
    pub nonce: Nonce,
    pub issued_at_ms: u64,
    pub expires_at_ms: u64,
}
signed_object!(FreshnessChallenge, "freshness-challenge-v2");
impl FreshnessChallenge {
    pub fn new(
        auth: &VerifiedAuthorization,
        stage: CheckStage,
        nonce: Nonce,
        issued_at_ms: u64,
        expires_at_ms: u64,
    ) -> Result<Self> {
        require(
            match auth.phase {
                AuthorizationPhase::Admission => {
                    matches!(stage, CheckStage::Admit | CheckStage::RemoteRead)
                }
                AuthorizationPhase::Operation => {
                    matches!(stage, CheckStage::Start | CheckStage::Renew)
                }
            },
            "freshness stage requires matching admission state",
        )?;
        require(
            issued_at_ms >= auth.verified_at_ms,
            "challenge clock moved backwards",
        )?;
        validate_window(issued_at_ms, expires_at_ms, issued_at_ms)?;
        require(
            expires_at_ms - issued_at_ms <= stage.max_age(),
            "challenge lifetime too long",
        )?;
        require(
            stage != CheckStage::RemoteRead || auth.invocation.payload.action.is_read(),
            "write requires fresh stage",
        )?;
        Ok(Self {
            version: AUTH_VERSION,
            stage,
            invocation_hash: auth.invocation_hash.clone(),
            operation_id: auth.operation_id().to_owned(),
            body_hash: auth.body_hash().clone(),
            audience: auth.audience().clone(),
            presenter: auth.invocation.payload.presenter.clone(),
            policies: auth.dependencies.clone(),
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
impl CheckStage {
    fn max_age(self) -> u64 {
        if self == Self::RemoteRead {
            REMOTE_READ_MAX_AGE_MS
        } else {
            EXECUTION_LEASE_MS
        }
    }
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct FreshnessReceipt {
    pub version: u16,
    pub current: PolicyStamp,
    pub challenge_hash: ObjectHash,
    pub issued_at_ms: u64,
    pub expires_at_ms: u64,
}
signed_object!(FreshnessReceipt, "freshness-v2");

/// Construct from the parsed method and database-owned resource mapping.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RequestFacts {
    pub resource: ResourceRef,
    pub action: Action,
    pub audience: Audience,
    pub operation_id: String,
    pub body_hash: ObjectHash,
    pub context: InvocationContext,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TransportEvidence {
    DirectNoise {
        device: DeviceId,
    },
    GatewayNoise {
        device: DeviceId,
        gateway: DeviceId,
    },
    VerifiedPeer {
        origin: DeviceId,
    },
    LocalAgentAdapter {
        service: ServiceId,
        session: SessionRef,
    },
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AuthorizationPhase {
    Admission,
    Operation,
}
#[derive(Clone, Debug)]
pub struct VerifiedAdmission {
    invocation: Signed<Invocation>,
    admitted_at_ms: u64,
}
impl VerifiedAdmission {
    pub fn signed_invocation(&self) -> &Signed<Invocation> {
        &self.invocation
    }
    pub fn admitted_at_ms(&self) -> u64 {
        self.admitted_at_ms
    }
}
#[derive(Clone, Debug)]
pub struct VerifiedAuthorization {
    invocation: Signed<Invocation>,
    invocation_hash: ObjectHash,
    dependencies: Vec<PolicyStamp>,
    grant_path: Vec<ObjectHash>,
    budget: Budget,
    phase: AuthorizationPhase,
    verified_at_ms: u64,
}
impl VerifiedAuthorization {
    pub fn signed_invocation(&self) -> &Signed<Invocation> {
        &self.invocation
    }
    pub fn actor(&self) -> &Actor {
        &self.invocation.payload.actor
    }
    pub fn initiator(&self) -> &Actor {
        &self.invocation.payload.initiator
    }
    pub fn device(&self) -> &DeviceId {
        &self.invocation.payload.device
    }
    pub fn resource(&self) -> &ResourceRef {
        &self.invocation.payload.resource
    }
    pub fn action(&self) -> Action {
        self.invocation.payload.action
    }
    pub fn audience(&self) -> &Audience {
        &self.invocation.payload.audience
    }
    pub fn operation_id(&self) -> &str {
        &self.invocation.payload.operation_id
    }
    pub fn body_hash(&self) -> &ObjectHash {
        &self.invocation.payload.body_hash
    }
    pub fn invocation_hash(&self) -> &ObjectHash {
        &self.invocation_hash
    }
    pub fn dependencies(&self) -> &[PolicyStamp] {
        &self.dependencies
    }
    pub fn grant_path(&self) -> &[ObjectHash] {
        &self.grant_path
    }
    pub fn nonce(&self) -> &Nonce {
        &self.invocation.payload.nonce
    }
    pub fn expires_at_ms(&self) -> u64 {
        if self.phase == AuthorizationPhase::Operation {
            self.budget.expires_at_ms
        } else {
            self.invocation
                .payload
                .expires_at_ms
                .min(self.budget.expires_at_ms)
        }
    }
}
#[derive(Clone, Debug)]
pub struct VerifiedFreshness {
    verified_at_ms: u64,
    invocation_hash: ObjectHash,
    challenge_hash: ObjectHash,
    nonce: Nonce,
    stage: CheckStage,
    expires_at_ms: u64,
    dependencies: Vec<PolicyStamp>,
}
impl VerifiedFreshness {
    pub fn invocation_hash(&self) -> &ObjectHash {
        &self.invocation_hash
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
    pub fn expires_at_ms(&self) -> u64 {
        self.expires_at_ms
    }
    pub fn dependencies(&self) -> &[PolicyStamp] {
        &self.dependencies
    }
}

#[derive(Clone, Debug)]
struct AccountState {
    history: Vec<Signed<AccountDocument>>,
    stamp: PolicyStamp,
}
#[derive(Clone, Debug)]
struct SpaceState {
    history: Vec<Signed<SpaceDocument>>,
    stamp: PolicyStamp,
}
/// Rebuild by replaying signed documents from locally pinned genesis keys.
#[derive(Clone, Debug, Default)]
pub struct VerifiedPolicySet {
    accounts: BTreeMap<AccountId, AccountState>,
    spaces: BTreeMap<SpaceId, SpaceState>,
    devices: BTreeMap<ObjectHash, Signed<DeviceCertificate>>,
    resources: BTreeMap<ObjectHash, Signed<ResourceRecord>>,
    authenticators: BTreeMap<ObjectHash, Signed<AuthenticatorRecord>>,
    memberships: BTreeMap<ObjectHash, Signed<Membership>>,
    grants: BTreeMap<ObjectHash, Signed<Grant>>,
    roots: BTreeMap<PolicyStamp, PublicKey>,
    commits: BTreeMap<ObjectHash, Signed<PolicyCommit>>,
}
#[derive(Clone, Debug)]
struct Budget {
    objects: BTreeSet<ObjectHash>,
    signatures: BTreeSet<(PublicKey, String, ObjectHash, Signature)>,
    bytes: usize,
    expires_at_ms: u64,
    steps: usize,
    dependencies: BTreeMap<PolicyDomain, PolicyStamp>,
}
impl Default for Budget {
    fn default() -> Self {
        Self {
            objects: BTreeSet::new(),
            signatures: BTreeSet::new(),
            bytes: 0,
            expires_at_ms: u64::MAX,
            steps: 0,
            dependencies: BTreeMap::new(),
        }
    }
}
impl Budget {
    fn verify_raw(
        &mut self,
        key: &PublicKey,
        domain: &str,
        bytes: &[u8],
        signature: &Signature,
    ) -> Result<()> {
        let hash = ObjectHash(format!("h2_{}", digest(domain, bytes)));
        if self.signatures.contains(&(
            key.clone(),
            domain.to_owned(),
            hash.clone(),
            signature.clone(),
        )) {
            return Ok(());
        }
        if self.signatures.len() >= MAX_PROOF_SIGNATURES {
            return Err(AuthorizationError::BudgetExceeded);
        }
        crate::verify(key.as_str(), domain, bytes, signature.as_str())
            .map_err(|_| AuthorizationError::InvalidProof("invalid object signature"))?;
        self.signatures
            .insert((key.clone(), domain.to_owned(), hash, signature.clone()));
        Ok(())
    }
    fn expires(&mut self, deadline: u64) {
        self.expires_at_ms = self.expires_at_ms.min(deadline);
    }
    fn verify<T: SignedObject>(&mut self, value: &Signed<T>) -> Result<ObjectHash> {
        self.steps += 1;
        if self.steps > 128 {
            return Err(AuthorizationError::BudgetExceeded);
        }
        let hash = value.hash()?;
        if self.objects.insert(hash.clone()) {
            self.bytes += canonical(value)?.len();
            if self.objects.len() > MAX_PROOF_OBJECTS || self.bytes > MAX_PROOF_BYTES {
                return Err(AuthorizationError::BudgetExceeded);
            }
            value.verify_with_budget(self)?;
        }
        Ok(hash)
    }
}
fn sorted_unique<T: Ord>(values: &[T]) -> bool {
    values.windows(2).all(|pair| pair[0] < pair[1])
}
fn validate_window(start: u64, end: u64, now: u64) -> Result<()> {
    require(start < end, "invalid validity interval")?;
    if now < start || now >= end {
        return Err(AuthorizationError::Expired);
    }
    Ok(())
}
fn validate_context(context: &InvocationContext) -> Result<()> {
    for value in [
        context.conversation.as_deref(),
        context.executor_kind.as_deref(),
        context.sdk_session.as_ref().map(|s| s.session_id.as_str()),
        context
            .execution_session
            .as_ref()
            .map(|s| s.session_id.as_str()),
    ]
    .into_iter()
    .flatten()
    {
        require(
            !value.is_empty() && value.len() <= 256,
            "invalid invocation context",
        )?;
    }
    Ok(())
}
fn valid_operation_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_-:.".contains(&b))
}
fn same_or_tighter<T: PartialEq>(parent: &Option<T>, child: &Option<T>) -> bool {
    parent.is_none() || parent == child
}
fn context_tighter(parent: &InvocationContext, child: &InvocationContext) -> bool {
    same_or_tighter(&parent.conversation, &child.conversation)
        && same_or_tighter(&parent.task, &child.task)
        && same_or_tighter(&parent.sdk_session, &child.sdk_session)
        && same_or_tighter(&parent.execution_session, &child.execution_session)
        && same_or_tighter(&parent.executor_kind, &child.executor_kind)
        && same_or_tighter(&parent.workspace_resource, &child.workspace_resource)
}
fn scope_covers(scope: &ResourceScope, resource: &ResourceRef) -> bool {
    match scope {
        ResourceScope::WholeSpace(space) => space == &resource.space,
        ResourceScope::Exact(exact) => exact == resource,
    }
}
fn scope_tighter(parent: &ResourceScope, child: &ResourceScope) -> bool {
    match child {
        ResourceScope::Exact(resource) => scope_covers(parent, resource),
        ResourceScope::WholeSpace(_) => parent == child,
    }
}
fn verify_approval(
    key: &PublicKey,
    domain: &str,
    bytes: &[u8],
    signature: &Signature,
    budget: &mut Budget,
) -> Result<()> {
    budget.verify_raw(key, domain, bytes, signature)
}
fn recovery_approved(
    policy: &RecoveryPolicy,
    approvals: &[KeyApproval],
    domain: &str,
    bytes: &[u8],
    budget: &mut Budget,
) -> Result<()> {
    require(approvals.len() <= 8, "too many recovery approvals")?;
    let mut keys = BTreeSet::new();
    for approval in approvals {
        require(
            policy.keys.contains(&approval.key) && keys.insert(approval.key.clone()),
            "invalid recovery approver",
        )?;
        verify_approval(&approval.key, domain, bytes, &approval.signature, budget)?;
    }
    require(
        keys.len() >= usize::from(policy.threshold),
        "recovery threshold not met",
    )
}
fn advance(
    previous: &Option<PolicyStamp>,
    epoch: u64,
    sequence: u64,
    current: &PolicyStamp,
) -> Result<()> {
    if previous.as_ref() != Some(current) {
        return Err(AuthorizationError::PolicyConflict);
    }
    require(
        (epoch == current.authority_epoch && current.sequence.checked_add(1) == Some(sequence))
            || (current.authority_epoch.checked_add(1) == Some(epoch) && sequence == 0),
        "non-contiguous policy transition",
    )
}

impl VerifiedPolicySet {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn head(&self, domain: &PolicyDomain) -> Option<&PolicyStamp> {
        match domain {
            PolicyDomain::Account(id) => self.accounts.get(id).map(|s| &s.stamp),
            PolicyDomain::Space(id) => self.spaces.get(id).map(|s| &s.stamp),
        }
    }
    pub fn account(&self, id: &AccountId) -> Option<&AccountDocument> {
        self.accounts
            .get(id)
            .and_then(|s| s.history.last())
            .map(|s| &s.payload)
    }
    pub fn space(&self, id: &SpaceId) -> Option<&SpaceDocument> {
        self.spaces
            .get(id)
            .and_then(|s| s.history.last())
            .map(|s| &s.payload)
    }
    pub fn account_history(&self, id: &AccountId) -> Option<&[Signed<AccountDocument>]> {
        self.accounts.get(id).map(|s| s.history.as_slice())
    }
    pub fn space_history(&self, id: &SpaceId) -> Option<&[Signed<SpaceDocument>]> {
        self.spaces.get(id).map(|s| s.history.as_slice())
    }
    pub fn device(&self, hash: &ObjectHash) -> Option<&Signed<DeviceCertificate>> {
        self.devices.get(hash)
    }
    pub fn membership(&self, hash: &ObjectHash) -> Option<&Signed<Membership>> {
        self.memberships.get(hash)
    }
    pub fn grant(&self, hash: &ObjectHash) -> Option<&Signed<Grant>> {
        self.grants.get(hash)
    }
    pub fn resource(&self, space: &SpaceId, id: &ResourceId) -> Option<&Signed<ResourceRecord>> {
        self.space(space)?
            .resources
            .iter()
            .filter_map(|h| self.resources.get(h))
            .find(|r| r.payload.space == *space && r.payload.resource == *id)
    }
    /// Stores a signed object; only a current policy hash commitment authorizes its use.
    pub fn install_resource(&mut self, signed: Signed<ResourceRecord>) -> Result<ObjectHash> {
        require(self.resources.len() < 16384, "too many resource records")?;
        let r = &signed.payload;
        require(
            r.version == AUTH_VERSION
                && r.issued_under.domain == PolicyDomain::Space(r.space.clone()),
            "invalid resource record",
        )?;
        self.historical_root(&r.issued_under)?;
        signed.verify_signature()?;
        let hash = signed.hash()?;
        if self
            .space(&r.space)
            .is_some_and(|s| s.resources.contains(&hash))
        {
            require(
                self.resource(&r.space, &r.resource)
                    .is_none_or(|current| current.hash().ok().as_ref() == Some(&hash)),
                "conflicting registered resource",
            )?;
        }
        self.resources.insert(hash.clone(), signed);
        Ok(hash)
    }

    pub fn trust_account_genesis(
        &mut self,
        pinned_root: &PublicKey,
        signed: Signed<AccountDocument>,
    ) -> Result<PolicyStamp> {
        let d = &signed.payload;
        require(
            !self.accounts.contains_key(&d.account),
            "account already pinned",
        )?;
        require(
            &signed.issuer == pinned_root
                && &d.genesis_root == pinned_root
                && d.current_root == *pinned_root
                && d.account == AccountId::from_root(pinned_root),
            "account genesis pin mismatch",
        )?;
        require(
            d.previous.is_none()
                && d.authority_epoch == 0
                && d.sequence == 0
                && d.root_generation == 0
                && d.root_acceptance.is_none()
                && d.recovery_approvals.is_empty()
                && d.devices.is_empty()
                && d.revoked_devices.is_empty()
                && d.enrollment.is_empty()
                && d.authenticators.is_empty(),
            "invalid account genesis",
        )?;
        self.validate_account(d)?;
        let mut budget = Budget::default();
        signed.verify_with_budget(&mut budget)?;
        let stamp = account_stamp(&signed)?;
        self.roots
            .insert(stamp.clone(), signed.payload.current_root.clone());
        self.accounts.insert(
            d.account.clone(),
            AccountState {
                history: vec![signed],
                stamp: stamp.clone(),
            },
        );
        Ok(stamp)
    }
    pub fn install_account(&mut self, signed: Signed<AccountDocument>) -> Result<PolicyStamp> {
        self.apply_account(signed, None, Budget::default(), None)
    }
    fn apply_account(
        &mut self,
        signed: Signed<AccountDocument>,
        committed_at: Option<u64>,
        mut budget: Budget,
        commit: Option<(&Signed<PolicyCommit>, &[Signed<FreshnessReceipt>], u64)>,
    ) -> Result<PolicyStamp> {
        let d = &signed.payload;
        let state = self
            .accounts
            .get(&d.account)
            .ok_or(AuthorizationError::InvalidProof("account is not pinned"))?;
        let previous = &state
            .history
            .last()
            .ok_or(AuthorizationError::PolicyConflict)?
            .payload;
        require(
            state.history.len() < MAX_POLICY_HISTORY,
            "account history requires checkpoint",
        )?;
        advance(&d.previous, d.authority_epoch, d.sequence, &state.stamp)?;
        require(
            d.authority == previous.authority || d.authority_epoch > previous.authority_epoch,
            "authority replacement requires higher epoch",
        )?;
        for hash in &previous.devices {
            if !d.devices.contains(hash) {
                let Some(old) = self.devices.get(hash) else {
                    continue;
                };
                let replaced = d.devices.iter().any(|h| {
                    self.devices
                        .get(h)
                        .is_some_and(|c| c.payload.device == old.payload.device)
                });
                require(
                    replaced || d.revoked_devices.contains(&old.payload.device),
                    "removed device must remain revoked",
                )?;
            }
        }
        require(
            d.genesis_root == previous.genesis_root && d.account == previous.account,
            "account identity changed",
        )?;
        self.validate_account(d)?;
        for old in &previous.revoked_devices {
            require(d.revoked_devices.contains(old), "device revocation removed")?;
        }
        self.validate_device_registration(
            &PolicyDomain::Account(d.account.clone()),
            &d.devices,
            &d.revoked_devices,
            &d.current_root,
        )?;
        self.validate_authenticator_registration(d)?;
        let ordinary_device = signed.issuer != previous.current_root
            && !previous.recovery.keys.contains(&signed.issuer);
        if ordinary_device {
            self.authorize_account_delta(
                &signed,
                previous,
                committed_at.ok_or(AuthorizationError::InvalidProof(
                    "daily governance requires authority commit",
                ))?,
                &mut budget,
            )?;
        } else {
            let bytes = d.governance_bytes()?;
            validate_root_transition(
                &signed.issuer,
                &previous.current_root,
                &d.current_root,
                previous.root_generation,
                d.root_generation,
                &d.root_acceptance,
                "account-root-accept-v2",
                &bytes,
                &mut budget,
            )?;
            let recovery = signed.issuer != previous.current_root;
            if recovery || d.recovery != previous.recovery {
                recovery_approved(
                    &previous.recovery,
                    &d.recovery_approvals,
                    "account-recovery-v2",
                    &bytes,
                    &mut budget,
                )?;
            } else {
                require(
                    d.recovery_approvals.is_empty(),
                    "unexpected recovery approvals",
                )?;
            }
            if recovery {
                require(
                    previous.recovery.keys.contains(&signed.issuer)
                        && d.current_root != previous.current_root
                        && d.authority_epoch > previous.authority_epoch,
                    "invalid account recovery",
                )?;
            }
            validate_recovery_transition(&previous.recovery, &d.recovery)?;
        }
        signed.verify_with_budget(&mut budget)?;
        if let Some((commit, receipts, now)) = commit {
            self.verify_policy_receipts(
                commit,
                receipts,
                &PolicyDomain::Account(d.account.clone()),
                now,
                &mut budget,
            )?;
        }
        let stamp = account_stamp(&signed)?;
        self.roots
            .insert(stamp.clone(), signed.payload.current_root.clone());
        let state = self
            .accounts
            .get_mut(&d.account)
            .ok_or(AuthorizationError::PolicyConflict)?;
        state.history.push(signed);
        state.stamp = stamp.clone();
        Ok(stamp)
    }
    pub fn trust_space_genesis(
        &mut self,
        pinned_root: &PublicKey,
        signed: Signed<SpaceDocument>,
    ) -> Result<PolicyStamp> {
        let d = &signed.payload;
        require(!self.spaces.contains_key(&d.space), "space already pinned")?;
        require(
            &signed.issuer == pinned_root && &d.current_root == pinned_root,
            "space genesis pin mismatch",
        )?;
        require(
            d.previous.is_none()
                && d.authority_epoch == 0
                && d.sequence == 0
                && d.root_generation == 0
                && d.root_acceptance.is_none()
                && d.recovery_approvals.is_empty()
                && d.devices.is_empty()
                && d.revoked_devices.is_empty()
                && d.memberships.is_empty()
                && d.grants.is_empty()
                && d.governance_grant.is_none(),
            "invalid space genesis",
        )?;
        match &d.root {
            SpaceRoot::Personal {
                owner,
                genesis_root,
            } => {
                let account = self.account(owner).ok_or(AuthorizationError::InvalidProof(
                    "personal-space owner is not pinned",
                ))?;
                require(
                    d.space == SpaceId::personal(owner)
                        && genesis_root == &account.genesis_root
                        && pinned_root == &account.current_root,
                    "invalid personal-space root",
                )?;
            }
            SpaceRoot::Team { genesis_root } => require(
                d.space == SpaceId::team(genesis_root) && pinned_root == genesis_root,
                "invalid team-space root",
            )?,
        }
        self.validate_space(d)?;
        let mut budget = Budget::default();
        signed.verify_with_budget(&mut budget)?;
        let stamp = space_stamp(&signed)?;
        self.roots
            .insert(stamp.clone(), signed.payload.current_root.clone());
        self.spaces.insert(
            d.space.clone(),
            SpaceState {
                history: vec![signed],
                stamp: stamp.clone(),
            },
        );
        Ok(stamp)
    }
    pub fn install_space(&mut self, signed: Signed<SpaceDocument>) -> Result<PolicyStamp> {
        self.apply_space(signed, None, Budget::default(), None)
    }
    fn apply_space(
        &mut self,
        signed: Signed<SpaceDocument>,
        committed_at: Option<u64>,
        mut budget: Budget,
        commit: Option<(&Signed<PolicyCommit>, &[Signed<FreshnessReceipt>], u64)>,
    ) -> Result<PolicyStamp> {
        let d = &signed.payload;
        let state = self
            .spaces
            .get(&d.space)
            .ok_or(AuthorizationError::InvalidProof("space is not pinned"))?;
        let previous = &state
            .history
            .last()
            .ok_or(AuthorizationError::PolicyConflict)?
            .payload;
        require(
            state.history.len() < MAX_POLICY_HISTORY,
            "space history requires checkpoint",
        )?;
        advance(&d.previous, d.authority_epoch, d.sequence, &state.stamp)?;
        require(
            d.authority == previous.authority || d.authority_epoch > previous.authority_epoch,
            "authority replacement requires higher epoch",
        )?;
        for hash in &previous.devices {
            if !d.devices.contains(hash) {
                let Some(old) = self.devices.get(hash) else {
                    continue;
                };
                let replaced = d.devices.iter().any(|h| {
                    self.devices
                        .get(h)
                        .is_some_and(|c| c.payload.device == old.payload.device)
                });
                require(
                    replaced || d.revoked_devices.contains(&old.payload.device),
                    "removed device must remain revoked",
                )?;
            }
        }
        require(
            d.root == previous.root && d.space == previous.space,
            "space identity changed",
        )?;
        self.validate_space(d)?;
        for old in &previous.revoked_devices {
            require(d.revoked_devices.contains(old), "device revocation removed")?;
        }
        self.validate_device_registration(
            &PolicyDomain::Space(d.space.clone()),
            &d.devices,
            &d.revoked_devices,
            &d.current_root,
        )?;
        for hash in &d.grants {
            let Some(grant) = self.grants.get(hash) else {
                continue;
            };
            require(
                grant.payload.owner_space == d.space,
                "cross-space grant registration",
            )?;
        }
        for hash in &d.memberships {
            let Some(membership) = self.memberships.get(hash) else {
                continue;
            };
            require(
                membership.payload.space == d.space,
                "cross-space membership registration",
            )?;
        }
        for old in &state.history {
            for hash in &d.grants {
                require(
                    !old.payload.grants.contains(hash) || previous.grants.contains(hash),
                    "revoked grant cannot be reactivated",
                )?;
            }
            for hash in &d.memberships {
                require(
                    !old.payload.memberships.contains(hash) || previous.memberships.contains(hash),
                    "revoked membership cannot be reactivated",
                )?;
            }
        }
        let personal_owner = if let SpaceRoot::Personal { owner, .. } = &d.root {
            Some(
                self.account(owner)
                    .ok_or(AuthorizationError::PolicyConflict)?,
            )
        } else {
            None
        };
        let governing_root = personal_owner.map_or(&previous.current_root, |a| &a.current_root);
        let ordinary_device =
            &signed.issuer != governing_root && !previous.recovery.keys.contains(&signed.issuer);
        if ordinary_device {
            self.ensure_personal_current(previous)?;
            self.authorize_space_delta(
                &signed,
                previous,
                committed_at.ok_or(AuthorizationError::InvalidProof(
                    "daily governance requires authority commit",
                ))?,
                &mut budget,
            )?;
        } else if let Some(account) = personal_owner {
            require(
                signed.issuer == account.current_root
                    && d.current_root == account.current_root
                    && d.root_generation == account.root_generation
                    && d.root_acceptance.is_none()
                    && d.recovery_approvals.is_empty()
                    && d.recovery == account.recovery,
                "personal space requires current account root",
            )?;
        } else {
            let bytes = d.governance_bytes()?;
            validate_root_transition(
                &signed.issuer,
                &previous.current_root,
                &d.current_root,
                previous.root_generation,
                d.root_generation,
                &d.root_acceptance,
                "space-root-accept-v2",
                &bytes,
                &mut budget,
            )?;
            let recovery = signed.issuer != previous.current_root;
            if recovery || d.recovery != previous.recovery {
                recovery_approved(
                    &previous.recovery,
                    &d.recovery_approvals,
                    "space-recovery-v2",
                    &bytes,
                    &mut budget,
                )?;
            } else {
                require(
                    d.recovery_approvals.is_empty(),
                    "unexpected recovery approvals",
                )?;
            }
            if recovery {
                require(
                    previous.recovery.keys.contains(&signed.issuer)
                        && d.current_root != previous.current_root
                        && d.authority_epoch > previous.authority_epoch,
                    "invalid space recovery",
                )?;
            }
            validate_recovery_transition(&previous.recovery, &d.recovery)?;
        }
        signed.verify_with_budget(&mut budget)?;
        if let Some((commit, receipts, now)) = commit {
            self.verify_policy_receipts(
                commit,
                receipts,
                &PolicyDomain::Space(d.space.clone()),
                now,
                &mut budget,
            )?;
        }
        let stamp = space_stamp(&signed)?;
        self.roots
            .insert(stamp.clone(), signed.payload.current_root.clone());
        let state = self
            .spaces
            .get_mut(&d.space)
            .ok_or(AuthorizationError::PolicyConflict)?;
        state.history.push(signed);
        state.stamp = stamp.clone();
        Ok(stamp)
    }
    fn validate_account(&self, d: &AccountDocument) -> Result<()> {
        require(
            d.version == AUTH_VERSION
                && canonical(d)?.len() <= 262_144
                && d.devices.len() <= 1024
                && d.revoked_devices.len() <= 4096
                && d.enrollment.len() <= 1024
                && d.authenticators.len() <= 128
                && sorted_unique(&d.authenticators)
                && sorted_unique(&d.devices)
                && sorted_unique(&d.revoked_devices),
            "invalid account document",
        )?;
        require(
            d.enrollment.windows(2).all(|p| p[0].device < p[1].device),
            "duplicate enrollment authority",
        )?;
        require(
            d.enrollment
                .iter()
                .all(|e| !e.rights.is_empty() && e.rights.len() <= 3 && sorted_unique(&e.rights)),
            "invalid account enrollment rights",
        )?;
        require(
            !d.revoked_devices.contains(&d.authority.device),
            "revoked authority requires replacement",
        )?;
        d.authority.validate(&d.current_root)?;
        d.recovery.validate(&d.current_root)
    }
    fn validate_space(&self, d: &SpaceDocument) -> Result<()> {
        require(
            d.version == AUTH_VERSION
                && canonical(d)?.len() <= 262_144
                && d.devices.len() <= 1024
                && d.revoked_devices.len() <= 4096
                && d.grants.len() <= 4096
                && d.memberships.len() <= 1024
                && d.resources.len() <= 4096
                && sorted_unique(&d.devices)
                && sorted_unique(&d.revoked_devices)
                && sorted_unique(&d.grants)
                && sorted_unique(&d.memberships),
            "invalid space document",
        )?;
        require(sorted_unique(&d.resources), "duplicate resources")?;
        let mut known = BTreeSet::new();
        for hash in &d.resources {
            if let Some(record) = self.resources.get(hash) {
                require(
                    record.payload.space == d.space
                        && known.insert(record.payload.resource.clone()),
                    "invalid registered resource",
                )?;
            }
        }
        require(
            !d.revoked_devices.contains(&d.authority.device),
            "revoked authority requires replacement",
        )?;
        d.authority.validate(&d.current_root)?;
        d.recovery.validate(&d.current_root)
    }
    fn validate_device_registration(
        &self,
        domain: &PolicyDomain,
        hashes: &[ObjectHash],
        revoked: &[DeviceId],
        root: &PublicKey,
    ) -> Result<()> {
        let mut devices = BTreeSet::new();
        for hash in hashes {
            let Some(certificate) = self.devices.get(hash) else {
                continue;
            };
            require(
                &certificate.payload.owner == domain
                    && &certificate.payload.public_key != root
                    && !revoked.contains(&certificate.payload.device)
                    && devices.insert(&certificate.payload.device),
                "invalid registered device",
            )?;
        }
        Ok(())
    }
    fn historical_root(&self, stamp: &PolicyStamp) -> Result<PublicKey> {
        self.roots
            .get(stamp)
            .cloned()
            .ok_or_else(|| AuthorizationError::NeedPolicySync(vec![stamp.clone()]))
    }
    fn active_device_hashes(&self, domain: &PolicyDomain) -> Result<(&[ObjectHash], &[DeviceId])> {
        match domain {
            PolicyDomain::Account(id) => self
                .account(id)
                .map(|d| (d.devices.as_slice(), d.revoked_devices.as_slice())),
            PolicyDomain::Space(id) => self
                .space(id)
                .map(|d| (d.devices.as_slice(), d.revoked_devices.as_slice())),
        }
        .ok_or(AuthorizationError::InvalidProof("missing policy domain"))
    }
    fn find_device(&self, device: &DeviceId) -> Result<&Signed<DeviceCertificate>> {
        for (hash, certificate) in self
            .devices
            .iter()
            .filter(|(_, c)| &c.payload.device == device)
        {
            let (hashes, revoked) = self.active_device_hashes(&certificate.payload.owner)?;
            if !revoked.contains(device) && hashes.contains(hash) {
                return Ok(certificate);
            }
        }
        Err(AuthorizationError::Revoked)
    }
    fn dependency(
        &self,
        domain: &PolicyDomain,
        dependencies: &mut BTreeMap<PolicyDomain, PolicyStamp>,
    ) -> Result<()> {
        if let PolicyDomain::Space(id) = domain {
            let document = self
                .space(id)
                .ok_or(AuthorizationError::InvalidProof("missing space policy"))?;
            self.ensure_personal_current(document)?;
            if let SpaceRoot::Personal { owner, .. } = &document.root {
                self.dependency(&PolicyDomain::Account(owner.clone()), dependencies)?;
            }
        }
        let stamp = self
            .head(domain)
            .ok_or(AuthorizationError::InvalidProof("missing policy domain"))?;
        dependencies.insert(domain.clone(), stamp.clone());
        if dependencies.len() > MAX_POLICY_DOMAINS {
            return Err(AuthorizationError::BudgetExceeded);
        }
        Ok(())
    }
    pub fn install_device(
        &mut self,
        signed: Signed<DeviceCertificate>,
        now_ms: u64,
    ) -> Result<ObjectHash> {
        require(self.devices.len() < 4096, "too many historical devices")?;
        for existing in self
            .devices
            .values()
            .filter(|c| c.payload.device == signed.payload.device)
        {
            require(
                existing.payload.owner == signed.payload.owner
                    && existing.payload.subject == signed.payload.subject,
                "device key already belongs to another principal",
            )?;
        }
        self.validate_certificate(
            &signed,
            now_ms,
            &mut Budget::default(),
            &mut BTreeMap::new(),
            &mut BTreeSet::new(),
        )?;
        let hash = signed.hash()?;
        self.devices.insert(hash.clone(), signed);
        Ok(hash)
    }
    fn validate_certificate(
        &self,
        signed: &Signed<DeviceCertificate>,
        now_ms: u64,
        budget: &mut Budget,
        dependencies: &mut BTreeMap<PolicyDomain, PolicyStamp>,
        visiting: &mut BTreeSet<DeviceId>,
    ) -> Result<()> {
        let d = &signed.payload;
        require(
            visiting.len() < MAX_DELEGATION_DEPTH && visiting.insert(d.device.clone()),
            "cyclic device enrollment",
        )?;
        budget.verify(signed)?;
        require(
            d.version == AUTH_VERSION
                && d.device == DeviceId::from_key(&d.public_key)
                && !d.uses.is_empty()
                && d.uses.len() <= 4
                && sorted_unique(&d.uses)
                && d.issued_under.domain == d.owner,
            "invalid device certificate",
        )?;
        match &d.subject {
            Actor::Account(account) => require(
                d.owner == PolicyDomain::Account(account.clone())
                    && d.uses.contains(&DeviceUse::AccountInvocation)
                    && !d.uses.contains(&DeviceUse::ServiceInvocation),
                "account device subject mismatch",
            )?,
            Actor::Service(service) => require(
                *service == ServiceId::from_key(&d.public_key)
                    && d.uses.contains(&DeviceUse::ServiceInvocation)
                    && !d.uses.contains(&DeviceUse::AccountInvocation),
                "service device subject mismatch",
            )?,
        }
        validate_window(d.not_before_ms, d.expires_at_ms, now_ms)?;
        budget.expires(d.expires_at_ms);
        budget.verify_raw(
            &d.public_key,
            "device-possession-v2",
            &d.possession_bytes()?,
            d.possession
                .as_ref()
                .ok_or(AuthorizationError::InvalidProof(
                    "device possession required",
                ))?,
        )?;
        let root = self.historical_root(&d.issued_under)?;
        require(d.public_key != root, "root cannot be a device key")?;
        self.dependency(&d.owner, dependencies)?;
        if signed.issuer != root && matches!(d.owner, PolicyDomain::Space(_)) {
            let PolicyDomain::Space(space) = &d.owner else {
                unreachable!()
            };
            let hash = d
                .enrollment_grant
                .as_ref()
                .ok_or(AuthorizationError::InvalidProof(
                    "service enrollment requires governance grant",
                ))?;
            self.governance_capability(
                hash,
                &signed.issuer,
                space,
                Some(Action::ServiceEnroll),
                None,
                now_ms,
                budget,
            )?;
            let grant = &self
                .grants
                .get(hash)
                .ok_or(AuthorizationError::Revoked)?
                .payload;
            require(
                d.not_before_ms >= grant.not_before_ms
                    && d.expires_at_ms <= grant.expires_at_ms
                    && matches!(d.subject, Actor::Service(_)),
                "service certificate widens enrollment grant",
            )?;
            budget.expires(grant.expires_at_ms);
        } else if signed.issuer != root {
            require(
                d.enrollment_grant.is_none(),
                "account enrollment uses account rights",
            )?;
            let issuer_device = DeviceId::from_key(&signed.issuer);
            let issuer = self.find_device(&issuer_device)?;
            require(
                issuer.payload.owner == d.owner
                    && issuer.payload.uses.contains(&DeviceUse::EnrollDevice),
                "device signer cannot enroll",
            )?;
            let PolicyDomain::Account(account) = &d.owner else {
                return Err(AuthorizationError::InvalidProof(
                    "team enrollment requires root approval",
                ));
            };
            let authority = self
                .account(account)
                .and_then(|a| a.enrollment.iter().find(|e| e.device == issuer_device))
                .ok_or(AuthorizationError::Revoked)?;
            require(
                now_ms < authority.expires_at_ms
                    && d.expires_at_ms <= authority.expires_at_ms
                    && d.not_before_ms >= issuer.payload.not_before_ms
                    && d.expires_at_ms <= issuer.payload.expires_at_ms,
                "enrollment authority expired",
            )?;
            require(
                authority.rights.contains(&AccountRight::EnrollDevice),
                "device lacks enrollment right",
            )?;
            budget.expires(authority.expires_at_ms);
            self.validate_certificate(issuer, now_ms, budget, dependencies, visiting)?;
        }
        budget.dependencies.extend(dependencies.clone());
        visiting.remove(&d.device);
        Ok(())
    }
    pub fn install_membership(
        &mut self,
        signed: Signed<Membership>,
        now_ms: u64,
    ) -> Result<ObjectHash> {
        require(self.memberships.len() < 4096, "too many memberships")?;
        let m = &signed.payload;
        require(
            m.version == AUTH_VERSION
                && m.issued_under.domain == PolicyDomain::Space(m.space.clone())
                && !m.roles.is_empty()
                && m.roles.len() <= 4
                && sorted_unique(&m.roles),
            "invalid membership",
        )?;
        validate_window(m.not_before_ms, m.expires_at_ms, now_ms)?;
        if signed.issuer != self.historical_root(&m.issued_under)? {
            self.actor_for_signer(
                &signed.issuer,
                now_ms,
                &mut Budget::default(),
                &mut BTreeMap::new(),
            )?;
        }
        signed.verify_signature()?;
        let hash = signed.hash()?;
        self.memberships.insert(hash.clone(), signed);
        Ok(hash)
    }
    pub fn install_grant(&mut self, signed: Signed<Grant>, now_ms: u64) -> Result<ObjectHash> {
        require(self.grants.len() < 16384, "too many grants")?;
        let mut path = self.grant_chain(signed.payload.parent.as_ref())?;
        path.push(&signed);
        self.verify_grant_path(
            &path,
            now_ms,
            &mut Budget::default(),
            &mut BTreeMap::new(),
            false,
        )?;
        let hash = signed.hash()?;
        self.grants.insert(hash.clone(), signed);
        Ok(hash)
    }
    fn grant_chain(&self, leaf: Option<&ObjectHash>) -> Result<Vec<&Signed<Grant>>> {
        let mut path = Vec::new();
        let mut cursor = leaf;
        let mut visited = BTreeSet::new();
        while let Some(hash) = cursor {
            require(
                path.len() < MAX_DELEGATION_DEPTH && visited.insert(hash.clone()),
                "cyclic or deep grant chain",
            )?;
            let grant = self
                .grants
                .get(hash)
                .ok_or(AuthorizationError::InvalidProof("missing installed grant"))?;
            cursor = grant.payload.parent.as_ref();
            path.push(grant);
        }
        path.reverse();
        Ok(path)
    }
    fn actor_for_signer(
        &self,
        key: &PublicKey,
        now_ms: u64,
        budget: &mut Budget,
        dependencies: &mut BTreeMap<PolicyDomain, PolicyStamp>,
    ) -> Result<Actor> {
        let certificate = self.find_device(&DeviceId::from_key(key))?;
        require(
            certificate.payload.public_key == *key,
            "device signing key mismatch",
        )?;
        self.validate_certificate(
            certificate,
            now_ms,
            budget,
            dependencies,
            &mut BTreeSet::new(),
        )?;
        Ok(certificate.payload.subject.clone())
    }
    fn matches_grantee(
        &self,
        grantee: &Grantee,
        actor: &Actor,
        now_ms: u64,
        budget: &mut Budget,
        dependencies: &mut BTreeMap<PolicyDomain, PolicyStamp>,
    ) -> Result<bool> {
        match grantee {
            Grantee::Actor(expected) => Ok(expected == actor),
            Grantee::MembersOf(selector) => {
                require(
                    !selector.roles_any.is_empty()
                        && selector.roles_any.len() <= 4
                        && sorted_unique(&selector.roles_any),
                    "invalid membership selector",
                )?;
                if !matches!(
                    (&selector.kind, actor),
                    (MemberKind::Account, Actor::Account(_))
                        | (MemberKind::Service, Actor::Service(_))
                ) {
                    return Ok(false);
                }
                let state = self
                    .spaces
                    .get(&selector.space)
                    .ok_or(AuthorizationError::InvalidProof("missing selector space"))?;
                require(
                    state
                        .history
                        .first()
                        .ok_or(AuthorizationError::PolicyConflict)?
                        .hash()?
                        == selector.pinned_genesis,
                    "selector genesis mismatch",
                )?;
                self.dependency(&PolicyDomain::Space(selector.space.clone()), dependencies)?;
                let current = &state
                    .history
                    .last()
                    .ok_or(AuthorizationError::PolicyConflict)?
                    .payload;
                for hash in &current.memberships {
                    let Some(membership) = self.memberships.get(hash) else {
                        continue;
                    };
                    let m = &membership.payload;
                    if &m.subject != actor
                        || !m.roles.iter().any(|role| selector.roles_any.contains(role))
                    {
                        continue;
                    }
                    match validate_window(m.not_before_ms, m.expires_at_ms, now_ms) {
                        Err(AuthorizationError::Expired) => continue,
                        result => result?,
                    }
                    budget.verify(membership)?;
                    budget.expires(m.expires_at_ms);
                    self.historical_root(&m.issued_under)?;
                    if membership.issuer != self.historical_root(&m.issued_under)? {
                        self.actor_for_signer(&membership.issuer, now_ms, budget, dependencies)?;
                    }
                    budget.dependencies.extend(dependencies.clone());
                    return Ok(true);
                }
                Ok(false)
            }
        }
    }
    fn verify_grant_path(
        &self,
        path: &[&Signed<Grant>],
        now_ms: u64,
        budget: &mut Budget,
        dependencies: &mut BTreeMap<PolicyDomain, PolicyStamp>,
        require_registered_leaf: bool,
    ) -> Result<(Option<(Actor, ObjectHash)>, Vec<ObjectHash>)> {
        require(
            !path.is_empty() && path.len() <= MAX_DELEGATION_DEPTH,
            "invalid grant depth",
        )?;
        let mut initiating = None;
        let mut hashes = Vec::new();
        for (index, signed) in path.iter().enumerate() {
            let g = &signed.payload;
            let hash = budget.verify(*signed)?;
            require(
                g.version == AUTH_VERSION
                    && !g.actions.is_empty()
                    && g.actions.len() <= 32
                    && sorted_unique(&g.actions)
                    && g.remaining_delegations <= MAX_DELEGATION_DEPTH as u8
                    && g.issued_under.domain == PolicyDomain::Space(g.owner_space.clone()),
                "invalid grant",
            )?;
            validate_window(g.not_before_ms, g.expires_at_ms, now_ms)?;
            budget.expires(g.expires_at_ms);
            validate_context(&g.constraints.context)?;
            match &g.scope {
                ResourceScope::WholeSpace(space) => {
                    require(space == &g.owner_space, "cross-space grant scope")?
                }
                ResourceScope::Exact(resource) => {
                    require(resource.space == g.owner_space, "cross-space grant scope")?
                }
            }
            self.dependency(&g.issued_under.domain, dependencies)?;
            let space = self
                .space(&g.owner_space)
                .ok_or(AuthorizationError::InvalidProof("missing resource space"))?;
            if require_registered_leaf || index + 1 != path.len() {
                if !space.grants.contains(&hash) {
                    return Err(AuthorizationError::Revoked);
                }
            }
            self.historical_root(&g.issued_under)?;
            if index == 0 {
                require(
                    g.parent.is_none() && signed.issuer == self.historical_root(&g.issued_under)?,
                    "root grant requires space root",
                )?;
            } else {
                let parent = &path[index - 1].payload;
                require(
                    g.parent.as_ref() == hashes.last() && parent.owner_space == g.owner_space,
                    "grant parent mismatch",
                )?;
                let signer = self.actor_for_signer(&signed.issuer, now_ms, budget, dependencies)?;
                require(
                    self.matches_grantee(&parent.grantee, &signer, now_ms, budget, dependencies)?,
                    "grant issuer is not parent grantee",
                )?;
                require(
                    matches!(g.grantee, Grantee::Actor(_))
                        && scope_tighter(&parent.scope, &g.scope)
                        && g.actions.iter().all(|a| parent.actions.contains(a))
                        && g.audience == parent.audience
                        && context_tighter(&parent.constraints.context, &g.constraints.context)
                        && (!parent.constraints.require_account_actor
                            || g.constraints.require_account_actor)
                        && (!parent.constraints.require_webauthn_uv
                            || g.constraints.require_webauthn_uv)
                        && g.not_before_ms >= parent.not_before_ms
                        && g.expires_at_ms <= parent.expires_at_ms
                        && g.remaining_delegations < parent.remaining_delegations,
                    "delegation widens parent grant",
                )?;
                if let (Actor::Account(_), Grantee::Actor(Actor::Service(_))) =
                    (&signer, &g.grantee)
                {
                    if initiating.is_none() {
                        initiating = Some((signer, hash.clone()));
                    }
                }
            }
            hashes.push(hash);
        }
        budget.dependencies.extend(dependencies.clone());
        Ok((initiating, hashes))
    }
    pub fn verify_invocation(
        &self,
        signed: &Signed<Invocation>,
        facts: &RequestFacts,
        transport: &TransportEvidence,
        now_ms: u64,
    ) -> Result<VerifiedAuthorization> {
        self.verify_invocation_inner(
            signed,
            facts,
            transport,
            now_ms,
            AuthorizationPhase::Admission,
        )
    }
    /// The caller persists this proof with nonce consumption and policy-head CAS.
    pub fn admit_operation(
        &self,
        auth: &VerifiedAuthorization,
        freshness: &VerifiedFreshness,
        now_ms: u64,
    ) -> Result<VerifiedAdmission> {
        self.check_current(auth, now_ms)?;
        require(
            auth.phase == AuthorizationPhase::Admission
                && freshness.stage == CheckStage::Admit
                && freshness.invocation_hash == auth.invocation_hash
                && freshness.dependencies == auth.dependencies
                && now_ms < freshness.expires_at_ms
                && now_ms >= freshness.verified_at_ms,
            "admission freshness does not match invocation",
        )?;
        Ok(VerifiedAdmission {
            invocation: auth.invocation.clone(),
            admitted_at_ms: now_ms,
        })
    }
    pub fn verify_operation_authorization(
        &self,
        admission: &VerifiedAdmission,
        facts: &RequestFacts,
        transport: &TransportEvidence,
        now_ms: u64,
    ) -> Result<VerifiedAuthorization> {
        require(
            now_ms >= admission.admitted_at_ms,
            "operation clock moved backwards",
        )?;
        self.verify_invocation_inner(
            &admission.invocation,
            facts,
            transport,
            now_ms,
            AuthorizationPhase::Operation,
        )
    }
    fn verify_invocation_inner(
        &self,
        signed: &Signed<Invocation>,
        facts: &RequestFacts,
        transport: &TransportEvidence,
        now_ms: u64,
        phase: AuthorizationPhase,
    ) -> Result<VerifiedAuthorization> {
        let mut budget = Budget::default();
        let invocation_hash = budget.verify(signed)?;
        self.verify_signed_invocation_body(
            signed,
            invocation_hash,
            budget,
            facts,
            transport,
            now_ms,
            phase,
        )
    }
    fn verify_signed_invocation_body(
        &self,
        signed: &Signed<Invocation>,
        invocation_hash: ObjectHash,
        mut budget: Budget,
        facts: &RequestFacts,
        transport: &TransportEvidence,
        now_ms: u64,
        phase: AuthorizationPhase,
    ) -> Result<VerifiedAuthorization> {
        let i = &signed.payload;
        require(
            i.version == AUTH_VERSION
                && valid_operation_id(&i.operation_id)
                && i.resource == facts.resource
                && i.action == facts.action
                && i.audience == facts.audience
                && i.operation_id == facts.operation_id
                && i.body_hash == facts.body_hash
                && i.context == facts.context,
            "invocation does not match parsed request",
        )?;
        validate_context(&i.context)?;
        require(
            i.not_before_ms < i.expires_at_ms,
            "invalid invocation validity interval",
        )?;
        if phase == AuthorizationPhase::Admission {
            validate_window(i.not_before_ms, i.expires_at_ms, now_ms)?;
        }
        require(
            i.expires_at_ms - i.not_before_ms <= INVOCATION_MAX_AGE_MS,
            "invocation lifetime too long",
        )?;
        require(
            i.expected_policies.len() <= MAX_POLICY_DOMAINS
                && i.expected_policies
                    .windows(2)
                    .all(|p| p[0].domain < p[1].domain),
            "invalid policy dependency set",
        )?;
        for stamp in &i.expected_policies {
            if phase == AuthorizationPhase::Admission && self.head(&stamp.domain) != Some(stamp) {
                return Err(AuthorizationError::NeedPolicySync(vec![stamp.clone()]));
            }
        }
        let certificate = self.find_device(&i.device)?;
        require(
            signed.issuer == certificate.payload.public_key
                && i.actor == certificate.payload.subject,
            "invocation actor does not own signing device",
        )?;
        let mut dependencies = BTreeMap::new();
        self.validate_certificate(
            certificate,
            now_ms,
            &mut budget,
            &mut dependencies,
            &mut BTreeSet::new(),
        )?;
        require(
            !matches!(i.actor, Actor::Account(_)) || i.context.sdk_session.is_none(),
            "account invocation cannot claim SDK adapter session",
        )?;
        if let Some(session) = &i.context.execution_session {
            let target = self.find_device(&session.target)?;
            require(
                matches!(target.payload.subject, Actor::Service(_))
                    && target.payload.uses.contains(&DeviceUse::Noise),
                "execution target is not a service",
            )?;
            self.validate_certificate(
                target,
                now_ms,
                &mut budget,
                &mut dependencies,
                &mut BTreeSet::new(),
            )?;
        }
        require(
            !i.action.requires_account() || matches!(i.actor, Actor::Account(_)),
            "action requires account actor",
        )?;
        let mut transport_dependencies = BTreeMap::new();
        let prior_budget_dependencies = budget.dependencies.clone();
        match transport {
            TransportEvidence::DirectNoise { device } => require(
                device == &i.device && i.presenter == *device,
                "direct transport device mismatch",
            )?,
            TransportEvidence::GatewayNoise { device, gateway } => {
                require(
                    device == &i.device && gateway == &i.presenter,
                    "gateway transport device mismatch",
                )?;
                self.validate_presenter(gateway, now_ms, &mut budget, &mut transport_dependencies)?;
            }
            TransportEvidence::VerifiedPeer { origin } => {
                require(origin == &i.presenter, "peer presenter mismatch")?;
                self.validate_presenter(origin, now_ms, &mut budget, &mut transport_dependencies)?;
            }
            TransportEvidence::LocalAgentAdapter { service, session } => require(
                i.actor == Actor::Service(service.clone())
                    && i.presenter == i.device
                    && i.context.sdk_session.as_ref() == Some(session),
                "agent session mismatch",
            )?,
        }
        transport_dependencies.extend(budget.dependencies.clone());
        budget.dependencies = prior_budget_dependencies;
        let resource = self
            .resource(&i.resource.space, &i.resource.resource)
            .ok_or(AuthorizationError::InvalidProof(
                "missing registered resource",
            ))?;
        budget.verify(resource)?;
        require(
            resource.payload.host == i.audience.service,
            "resource is not hosted by audience",
        )?;
        let target = self.find_device(&i.audience.device)?;
        require(
            target.payload.subject == Actor::Service(i.audience.service.clone()),
            "audience service device mismatch",
        )?;
        self.validate_certificate(
            target,
            now_ms,
            &mut budget,
            &mut dependencies,
            &mut BTreeSet::new(),
        )?;
        let path = self.grant_chain(Some(&i.grant_leaf))?;
        let (initiating, grant_path) =
            self.verify_grant_path(&path, now_ms, &mut budget, &mut dependencies, true)?;
        let leaf = &path
            .last()
            .ok_or(AuthorizationError::InvalidProof("missing grant leaf"))?
            .payload;
        require(
            scope_covers(&leaf.scope, &i.resource)
                && leaf.actions.contains(&i.action)
                && leaf.audience == i.audience
                && self.matches_grantee(
                    &leaf.grantee,
                    &i.actor,
                    now_ms,
                    &mut budget,
                    &mut dependencies,
                )?,
            "grant does not authorize invocation",
        )?;
        require(
            context_tighter(&leaf.constraints.context, &i.context),
            "invocation violates grant context",
        )?;
        require(
            !leaf.constraints.require_account_actor || matches!(i.actor, Actor::Account(_)),
            "grant requires account actor",
        )?;
        if leaf.constraints.require_webauthn_uv {
            return Err(AuthorizationError::UserPresenceRequired);
        }
        let expected_initiator = match &i.actor {
            Actor::Account(_) => {
                require(
                    i.initiating_grant.is_none(),
                    "direct account cannot claim delegated initiator",
                )?;
                i.actor.clone()
            }
            Actor::Service(_) => match initiating {
                Some((actor, hash)) => {
                    require(
                        i.initiating_grant.as_ref() == Some(&hash),
                        "initiating grant mismatch",
                    )?;
                    actor
                }
                None => {
                    require(
                        i.initiating_grant.is_none(),
                        "system service cannot claim initiating grant",
                    )?;
                    i.actor.clone()
                }
            },
        };
        require(i.initiator == expected_initiator, "forged initiator")?;
        self.expand_authorities(now_ms, &mut budget, &mut dependencies)?;
        let signed_dependencies: Vec<_> = dependencies.values().cloned().collect();
        if phase == AuthorizationPhase::Admission && i.expected_policies != signed_dependencies {
            return Err(AuthorizationError::NeedPolicySync(signed_dependencies));
        }
        if transport_dependencies
            .iter()
            .any(|(domain, stamp)| dependencies.get(domain) != Some(stamp))
        {
            dependencies.extend(transport_dependencies);
            self.expand_authorities(now_ms, &mut budget, &mut dependencies)?;
        }
        let dependencies = dependencies.into_values().collect();
        Ok(VerifiedAuthorization {
            invocation: signed.clone(),
            invocation_hash,
            dependencies,
            grant_path,
            budget,
            phase,
            verified_at_ms: now_ms,
        })
    }
    /// Verifies service membership; per-operation rights still require an Invocation.
    pub fn verify_service_device(
        &self,
        space: &SpaceId,
        device: &DeviceId,
        now_ms: u64,
    ) -> Result<&Signed<DeviceCertificate>> {
        let document = self
            .space(space)
            .ok_or(AuthorizationError::InvalidProof("missing service space"))?;
        self.ensure_personal_current(document)?;
        let certificate = self.find_device(device)?;
        require(
            matches!(certificate.payload.subject, Actor::Service(_))
                && certificate.payload.uses.contains(&DeviceUse::Noise),
            "device is not a Noise service",
        )?;
        match &document.root {
            SpaceRoot::Personal { owner, .. } => require(
                certificate.payload.owner == PolicyDomain::Account(owner.clone()),
                "service belongs to another personal account",
            )?,
            SpaceRoot::Team { .. } => require(
                certificate.payload.owner == PolicyDomain::Space(space.clone())
                    && document.devices.contains(&certificate.hash()?),
                "service is not registered in team space",
            )?,
        }
        let mut budget = Budget::default();
        let mut dependencies = BTreeMap::new();
        self.dependency(&PolicyDomain::Space(space.clone()), &mut dependencies)?;
        self.validate_certificate(
            certificate,
            now_ms,
            &mut budget,
            &mut dependencies,
            &mut BTreeSet::new(),
        )?;
        self.expand_authorities(now_ms, &mut budget, &mut dependencies)?;
        Ok(certificate)
    }
    fn validate_presenter(
        &self,
        device: &DeviceId,
        now_ms: u64,
        budget: &mut Budget,
        dependencies: &mut BTreeMap<PolicyDomain, PolicyStamp>,
    ) -> Result<()> {
        let presenter = self.find_device(device)?;
        require(
            matches!(presenter.payload.subject, Actor::Service(_))
                && presenter.payload.uses.contains(&DeviceUse::Noise),
            "peer presenter is not a service",
        )?;
        self.validate_certificate(
            presenter,
            now_ms,
            budget,
            dependencies,
            &mut BTreeSet::new(),
        )
    }
    /// Call inside the same transaction that consumes the nonce and reserves work.
    pub fn check_current(&self, auth: &VerifiedAuthorization, now_ms: u64) -> Result<()> {
        require(
            now_ms >= auth.verified_at_ms,
            "authorization clock moved backwards",
        )?;
        validate_window(
            auth.invocation.payload.not_before_ms,
            auth.expires_at_ms(),
            now_ms,
        )?;
        for stamp in &auth.dependencies {
            if self.head(&stamp.domain) != Some(stamp) {
                return Err(AuthorizationError::NeedPolicySync(vec![stamp.clone()]));
            }
        }
        Ok(())
    }
    pub fn verify_freshness(
        &self,
        auth: &VerifiedAuthorization,
        challenge: &FreshnessChallenge,
        receipts: &[Signed<FreshnessReceipt>],
        now_ms: u64,
    ) -> Result<VerifiedFreshness> {
        self.check_current(auth, now_ms)?;
        validate_window(challenge.issued_at_ms, challenge.expires_at_ms, now_ms)?;
        require(
            challenge.version == AUTH_VERSION
                && challenge.invocation_hash == auth.invocation_hash
                && challenge.operation_id == auth.operation_id()
                && challenge.body_hash == *auth.body_hash()
                && challenge.audience == *auth.audience()
                && challenge.presenter == auth.invocation.payload.presenter
                && challenge.policies == auth.dependencies
                && challenge.expires_at_ms - challenge.issued_at_ms <= challenge.stage.max_age(),
            "freshness challenge mismatch",
        )?;
        validate_window(challenge.issued_at_ms, challenge.expires_at_ms, now_ms)?;
        require(
            challenge.stage != CheckStage::RemoteRead || auth.action().is_read(),
            "write requires execution freshness",
        )?;
        let challenge_hash = challenge.hash()?;
        if receipts.len() != auth.dependencies.len() {
            return Err(AuthorizationError::NeedFreshness(
                auth.dependencies.iter().map(|s| s.domain.clone()).collect(),
            ));
        }
        let mut seen = BTreeSet::new();
        let mut expires_at_ms = challenge.expires_at_ms.min(auth.expires_at_ms());
        let mut budget = auth.budget.clone();
        for signed in receipts {
            budget.verify(signed)?;
            let r = &signed.payload;
            require(
                r.version == AUTH_VERSION
                    && r.challenge_hash == challenge_hash
                    && auth.dependencies.contains(&r.current)
                    && seen.insert(r.current.domain.clone()),
                "freshness receipt mismatch",
            )?;
            let authority = match &r.current.domain {
                PolicyDomain::Account(id) => self.account(id).map(|d| &d.authority),
                PolicyDomain::Space(id) => self.space(id).map(|d| &d.authority),
            }
            .ok_or(AuthorizationError::InvalidProof(
                "missing freshness authority",
            ))?;
            if self
                .devices
                .values()
                .any(|c| c.payload.device == authority.device)
            {
                self.find_device(&authority.device)?;
            }
            require(
                signed.issuer == authority.public_key
                    && r.issued_at_ms >= challenge.issued_at_ms
                    && r.expires_at_ms <= challenge.expires_at_ms,
                "receipt is not from current authority",
            )?;
            validate_window(r.issued_at_ms, r.expires_at_ms, now_ms)?;
            expires_at_ms = expires_at_ms.min(r.expires_at_ms);
        }
        require(
            match auth.phase {
                AuthorizationPhase::Admission => {
                    matches!(challenge.stage, CheckStage::Admit | CheckStage::RemoteRead)
                }
                AuthorizationPhase::Operation => {
                    matches!(challenge.stage, CheckStage::Start | CheckStage::Renew)
                }
            },
            "freshness stage requires matching admission state",
        )?;
        Ok(VerifiedFreshness {
            verified_at_ms: now_ms,
            invocation_hash: auth.invocation_hash.clone(),
            challenge_hash,
            nonce: challenge.nonce.clone(),
            stage: challenge.stage,
            expires_at_ms,
            dependencies: auth.dependencies.clone(),
        })
    }
}
fn account_stamp(signed: &Signed<AccountDocument>) -> Result<PolicyStamp> {
    let d = &signed.payload;
    Ok(PolicyStamp {
        domain: PolicyDomain::Account(d.account.clone()),
        authority_epoch: d.authority_epoch,
        sequence: d.sequence,
        head: signed.hash()?,
    })
}
fn space_stamp(signed: &Signed<SpaceDocument>) -> Result<PolicyStamp> {
    let d = &signed.payload;
    Ok(PolicyStamp {
        domain: PolicyDomain::Space(d.space.clone()),
        authority_epoch: d.authority_epoch,
        sequence: d.sequence,
        head: signed.hash()?,
    })
}
fn validate_root_transition(
    issuer: &PublicKey,
    old_root: &PublicKey,
    new_root: &PublicKey,
    old_generation: u64,
    new_generation: u64,
    acceptance: &Option<Signature>,
    domain: &str,
    bytes: &[u8],
    budget: &mut Budget,
) -> Result<()> {
    if new_root == old_root {
        require(
            issuer == old_root && old_generation == new_generation && acceptance.is_none(),
            "invalid unchanged root",
        )
    } else {
        require(
            old_generation.checked_add(1) == Some(new_generation),
            "root generation must advance",
        )?;
        verify_approval(
            new_root,
            domain,
            bytes,
            acceptance.as_ref().ok_or(AuthorizationError::InvalidProof(
                "new root must prove possession",
            ))?,
            budget,
        )
    }
}
fn validate_recovery_transition(old: &RecoveryPolicy, new: &RecoveryPolicy) -> Result<()> {
    require(
        if old == new {
            true
        } else {
            old.generation.checked_add(1) == Some(new.generation)
        },
        "recovery generation must advance",
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    pub(super) const NOW: u64 = 1000;
    fn key(byte: u8) -> Identity {
        Identity::from_secret_hex(&hex::encode([byte; 32])).unwrap()
    }
    pub(super) struct Fixture {
        pub(super) state: VerifiedPolicySet,
        pub(super) root: Identity,
        recovery: Identity,
        authority: Identity,
        user: Identity,
        pub(super) node: Identity,
        pub(super) account: AccountId,
        space: SpaceId,
        resource: ResourceRef,
        audience: Audience,
        root_grant: ObjectHash,
    }
    impl Fixture {
        pub(super) fn new() -> Self {
            let root = key(1);
            let recovery = key(2);
            let authority = key(3);
            let user = key(4);
            let node = key(5);
            let account = AccountId::from_root(&PublicKey::of(&root));
            let space = SpaceId::personal(&account);
            let resource = ResourceRef {
                space: space.clone(),
                resource: ResourceId::from_bytes([10; 32]),
            };
            let audience = Audience {
                service: ServiceId::from_key(&PublicKey::of(&node)),
                device: DeviceId::from_key(&PublicKey::of(&node)),
            };
            let mut state = VerifiedPolicySet::new();
            let account_genesis = Signed::sign(
                AccountDocument {
                    version: AUTH_VERSION,
                    account: account.clone(),
                    genesis_root: PublicKey::of(&root),
                    current_root: PublicKey::of(&root),
                    root_generation: 0,
                    previous: None,
                    authority_epoch: 0,
                    sequence: 0,
                    authority: AuthorityBinding::from_key(PublicKey::of(&authority)),
                    recovery: RecoveryPolicy {
                        generation: 0,
                        keys: vec![PublicKey::of(&recovery)],
                        threshold: 1,
                    },
                    enrollment: vec![],
                    devices: vec![],
                    revoked_devices: vec![],
                    authenticators: vec![],
                    root_acceptance: None,
                    recovery_approvals: vec![],
                },
                &root,
            )
            .unwrap();
            let initial_account = state
                .trust_account_genesis(&PublicKey::of(&root), account_genesis)
                .unwrap();
            let mut hashes = vec![];
            for (identity, subject, uses) in [
                (
                    &user,
                    Actor::Account(account.clone()),
                    vec![DeviceUse::Noise, DeviceUse::AccountInvocation],
                ),
                (
                    &node,
                    Actor::Service(audience.service.clone()),
                    vec![DeviceUse::Noise, DeviceUse::ServiceInvocation],
                ),
            ] {
                let mut certificate = DeviceCertificate {
                    version: AUTH_VERSION,
                    owner: PolicyDomain::Account(account.clone()),
                    subject,
                    device: DeviceId::from_key(&PublicKey::of(identity)),
                    public_key: PublicKey::of(identity),
                    uses,
                    possession: None,
                    enrollment_grant: None,
                    issued_under: initial_account.clone(),
                    nonce: Nonce::from_bytes([6; 32]),
                    not_before_ms: 0,
                    expires_at_ms: 1_000_000,
                };
                certificate.prove_possession(identity).unwrap();
                hashes.push(
                    state
                        .install_device(Signed::sign(certificate, &root).unwrap(), NOW)
                        .unwrap(),
                );
            }
            hashes.sort();
            let mut document = state.account(&account).unwrap().clone();
            document.previous = Some(initial_account);
            document.sequence = 1;
            document.devices = hashes;
            state
                .install_account(Signed::sign(document, &root).unwrap())
                .unwrap();
            let document = SpaceDocument {
                version: AUTH_VERSION,
                space: space.clone(),
                root: SpaceRoot::Personal {
                    owner: account.clone(),
                    genesis_root: PublicKey::of(&root),
                },
                current_root: PublicKey::of(&root),
                root_generation: 0,
                previous: None,
                authority_epoch: 0,
                sequence: 0,
                authority: AuthorityBinding::from_key(PublicKey::of(&authority)),
                recovery: RecoveryPolicy {
                    generation: 0,
                    keys: vec![PublicKey::of(&recovery)],
                    threshold: 1,
                },
                devices: vec![],
                revoked_devices: vec![],
                memberships: vec![],
                grants: vec![],
                resources: vec![],
                governance_grant: None,
                root_acceptance: None,
                recovery_approvals: vec![],
            };
            let space_genesis = state
                .trust_space_genesis(
                    &PublicKey::of(&root),
                    Signed::sign(document, &root).unwrap(),
                )
                .unwrap();
            let resource_hash = state
                .install_resource(
                    Signed::sign(
                        ResourceRecord {
                            version: AUTH_VERSION,
                            space: space.clone(),
                            resource: resource.resource.clone(),
                            kind: ResourceKind::Project,
                            host: audience.service.clone(),
                            issued_under: space_genesis.clone(),
                            nonce: Nonce::from_bytes([10; 32]),
                        },
                        &root,
                    )
                    .unwrap(),
                )
                .unwrap();
            let grant = Grant {
                version: AUTH_VERSION,
                owner_space: space.clone(),
                scope: ResourceScope::Exact(resource.clone()),
                grantee: Grantee::Actor(Actor::Account(account.clone())),
                actions: vec![
                    Action::MessageAppend,
                    Action::TaskAccept,
                    Action::ExecutionSend,
                ],
                audience: audience.clone(),
                constraints: GrantConstraints::default(),
                parent: None,
                remaining_delegations: 3,
                issued_under: space_genesis,
                not_before_ms: 0,
                expires_at_ms: 1_000_000,
                nonce: Nonce::from_bytes([7; 32]),
            };
            let root_grant = state
                .install_grant(Signed::sign(grant, &root).unwrap(), NOW)
                .unwrap();
            let mut fixture = Self {
                state,
                root,
                recovery,
                authority,
                user,
                node,
                account,
                space,
                resource,
                audience,
                root_grant,
            };
            let root_grant = fixture.root_grant.clone();
            fixture.update_space(|d| {
                d.grants.push(root_grant);
                d.resources.push(resource_hash);
            });
            fixture
        }
        fn update_space(&mut self, change: impl FnOnce(&mut SpaceDocument)) {
            let mut d = self.state.space(&self.space).unwrap().clone();
            d.previous = self
                .state
                .head(&PolicyDomain::Space(self.space.clone()))
                .cloned();
            d.sequence += 1;
            change(&mut d);
            d.grants.sort();
            d.memberships.sort();
            d.devices.sort();
            d.resources.sort();
            self.state
                .install_space(Signed::sign(d, &self.root).unwrap())
                .unwrap();
        }
        fn policies(&self) -> Vec<PolicyStamp> {
            vec![
                self.state
                    .head(&PolicyDomain::Account(self.account.clone()))
                    .unwrap()
                    .clone(),
                self.state
                    .head(&PolicyDomain::Space(self.space.clone()))
                    .unwrap()
                    .clone(),
            ]
        }
        fn invocation(&self) -> Invocation {
            Invocation {
                version: AUTH_VERSION,
                actor: Actor::Account(self.account.clone()),
                device: DeviceId::from_key(&PublicKey::of(&self.user)),
                initiator: Actor::Account(self.account.clone()),
                initiating_grant: None,
                resource: self.resource.clone(),
                action: Action::MessageAppend,
                audience: self.audience.clone(),
                presenter: DeviceId::from_key(&PublicKey::of(&self.user)),
                operation_id: "op_1".to_owned(),
                body_hash: ObjectHash::body(&serde_json::json!({"text":"hello"})).unwrap(),
                grant_leaf: self.root_grant.clone(),
                expected_policies: self.policies(),
                context: InvocationContext::default(),
                nonce: Nonce::from_bytes([8; 32]),
                not_before_ms: 0,
                expires_at_ms: 90_000,
            }
        }
        fn facts(&self, i: &Invocation) -> RequestFacts {
            RequestFacts {
                resource: i.resource.clone(),
                action: i.action,
                audience: i.audience.clone(),
                operation_id: i.operation_id.clone(),
                body_hash: i.body_hash.clone(),
                context: i.context.clone(),
            }
        }
        fn check(
            &self,
            i: Invocation,
            identity: &Identity,
            now_ms: u64,
        ) -> Result<VerifiedAuthorization> {
            let facts = self.facts(&i);
            let transport = TransportEvidence::DirectNoise {
                device: i.device.clone(),
            };
            self.state.verify_invocation(
                &Signed::sign(i, identity).unwrap(),
                &facts,
                &transport,
                now_ms,
            )
        }
        fn child(&self) -> Grant {
            let mut child = self.state.grant(&self.root_grant).unwrap().payload.clone();
            child.grantee = Grantee::Actor(Actor::Service(self.audience.service.clone()));
            child.parent = Some(self.root_grant.clone());
            child.remaining_delegations = 2;
            child.nonce = Nonce::from_bytes([9; 32]);
            child.issued_under = self
                .state
                .head(&PolicyDomain::Space(self.space.clone()))
                .unwrap()
                .clone();
            child
        }
        fn delegate(&mut self) -> ObjectHash {
            let child = self
                .state
                .install_grant(Signed::sign(self.child(), &self.user).unwrap(), NOW)
                .unwrap();
            self.update_space(|d| d.grants.push(child.clone()));
            child
        }
        fn service_invocation(&self, child: &ObjectHash) -> Invocation {
            let mut i = self.invocation();
            i.actor = Actor::Service(self.audience.service.clone());
            i.device = self.audience.device.clone();
            i.presenter = i.device.clone();
            i.grant_leaf = child.clone();
            i.initiating_grant = Some(child.clone());
            i
        }
        fn committed_space(
            &self,
            proposal: Signed<SpaceDocument>,
            commit: Signed<PolicyCommit>,
        ) -> Committed<SpaceDocument> {
            let challenge = commit.challenge_hash().unwrap();
            let receipts = commit
                .payload
                .dependencies
                .iter()
                .map(|current| {
                    Signed::sign(
                        FreshnessReceipt {
                            version: AUTH_VERSION,
                            current: current.clone(),
                            challenge_hash: challenge.clone(),
                            issued_at_ms: NOW,
                            expires_at_ms: NOW + 10_000,
                        },
                        &self.authority,
                    )
                    .unwrap()
                })
                .collect();
            Committed {
                proposal,
                commit,
                receipts,
            }
        }
        fn receipts(
            &self,
            auth: &VerifiedAuthorization,
            challenge: &FreshnessChallenge,
        ) -> Vec<Signed<FreshnessReceipt>> {
            auth.dependencies()
                .iter()
                .map(|stamp| {
                    Signed::sign(
                        FreshnessReceipt {
                            version: AUTH_VERSION,
                            current: stamp.clone(),
                            challenge_hash: challenge.hash().unwrap(),
                            issued_at_ms: NOW,
                            expires_at_ms: NOW + 10_000,
                        },
                        &self.authority,
                    )
                    .unwrap()
                })
                .collect()
        }
    }
    #[test]
    fn account_and_service_are_distinct_and_delegation_preserves_initiator() {
        let mut f = Fixture::new();
        let auth = f.check(f.invocation(), &f.user, NOW).unwrap();
        assert_eq!(auth.actor(), &Actor::Account(f.account.clone()));
        let child = f.delegate();
        let auth = f.check(f.service_invocation(&child), &f.node, NOW).unwrap();
        assert_eq!(auth.actor(), &Actor::Service(f.audience.service.clone()));
        assert_eq!(auth.initiator(), &Actor::Account(f.account.clone()));
        let mut forged = f.service_invocation(&child);
        forged.actor = Actor::Account(f.account.clone());
        assert_eq!(
            f.check(forged, &f.node, NOW).unwrap_err(),
            AuthorizationError::InvalidProof("invocation actor does not own signing device")
        );
        let mut forged = f.service_invocation(&child);
        forged.initiator = Actor::Service(f.audience.service.clone());
        assert_eq!(
            f.check(forged, &f.node, NOW).unwrap_err(),
            AuthorizationError::InvalidProof("forged initiator")
        );
        let mut forbidden = f.service_invocation(&child);
        forbidden.action = Action::TaskAccept;
        assert_eq!(
            f.check(forbidden, &f.node, NOW).unwrap_err(),
            AuthorizationError::InvalidProof("action requires account actor")
        );
    }
    #[test]
    fn endpoint_body_operation_and_expiry_are_bound() {
        let f = Fixture::new();
        let i = f.invocation();
        let signed = Signed::sign(i.clone(), &f.user).unwrap();
        let transport = TransportEvidence::DirectNoise {
            device: i.device.clone(),
        };
        for field in 0..4 {
            let mut facts = f.facts(&i);
            match field {
                0 => facts.audience.device = DeviceId::from_key(&PublicKey::of(&key(20))),
                1 => facts.audience.service = ServiceId::from_key(&PublicKey::of(&key(20))),
                2 => facts.body_hash = ObjectHash::body(&"different").unwrap(),
                _ => facts.operation_id = "op_2".into(),
            };
            assert_eq!(
                f.state
                    .verify_invocation(&signed, &facts, &transport, NOW)
                    .unwrap_err(),
                AuthorizationError::InvalidProof("invocation does not match parsed request")
            );
        }
        assert_eq!(
            f.check(i, &f.user, 90_000).unwrap_err(),
            AuthorizationError::Expired
        );
    }
    #[test]
    fn forwarded_invocation_binds_pinned_gateway_and_keeps_original_actor_and_request() {
        use super::bootstrap::*;
        let mut f = Fixture::new();
        let (original, mut pins) = client_bundle(&f);
        let auth = verify_client_authorization(
            &serde_json::to_string(&original).unwrap(),
            &serde_json::to_string(&pins).unwrap(),
            f.account.as_str(),
            f.space.as_str(),
            PublicKey::of(&f.user).as_str(),
            NOW as f64,
        )
        .unwrap();
        let gateway_root = key(21);
        let gateway_recovery = key(22);
        let gateway_client = key(23);
        let gateway = key(24);
        let nonce = Nonce::from_bytes([24; 32]);
        let claim_json =
            create_node_claim(&gateway, nonce.as_str(), NOW as f64, 100_000.0).unwrap();
        let claim = verify_node_claim(
            &claim_json,
            PublicKey::of(&gateway).as_str(),
            nonce.as_str(),
            NOW as f64,
        )
        .unwrap();
        let prepared = prepare_account_creation(
            &gateway_root,
            &gateway_recovery,
            &gateway_client,
            &claim,
            1_000_000.0,
            NOW as f64,
        )
        .unwrap();
        let pop = sign_node_possession(
            &prepared.node_request_json().unwrap(),
            &gateway,
            nonce.as_str(),
            NOW as f64,
        )
        .unwrap();
        let created = finish_account_creation(&prepared, &gateway_root, &pop, NOW as f64).unwrap();
        let gateway_bundle: ClientAuthorizationBundle =
            serde_json::from_str(&created.bundle_json().unwrap()).unwrap();
        let gateway_pins: Vec<PolicyPin> =
            serde_json::from_str(&created.pins_json().unwrap()).unwrap();
        pins.extend(gateway_pins);
        pins.sort_by(|a, b| a.domain.cmp(&b.domain));
        let mut journal = original.journal;
        journal.extend(gateway_bundle.journal);
        f.state = replay_policies(&pins, &journal).unwrap();
        let mut invocation = f.invocation();
        invocation.presenter = DeviceId::from_key(&PublicKey::of(&gateway));
        let template = serde_json::to_string(&invocation).unwrap();
        let body = r#"{"text":"hello"}"#;
        assert!(
            sign_client_invocation(
                &auth,
                &template,
                body,
                "message_append",
                "op_1",
                &f.user,
                NOW as f64
            )
            .is_err()
        );
        assert!(
            sign_forwarded_client_invocation(
                &auth,
                &template,
                body,
                "message_append",
                "op_1",
                PublicKey::of(&key(25)).as_str(),
                &f.user,
                NOW as f64
            )
            .is_err()
        );
        let signed: Signed<Invocation> = serde_json::from_str(
            &sign_forwarded_client_invocation(
                &auth,
                &template,
                body,
                "message_append",
                "op_1",
                PublicKey::of(&gateway).as_str(),
                &f.user,
                NOW as f64,
            )
            .unwrap(),
        )
        .unwrap();
        let facts = f.facts(&invocation);
        let transport = TransportEvidence::GatewayNoise {
            device: invocation.device.clone(),
            gateway: invocation.presenter.clone(),
        };
        let direct = f
            .state
            .verify_invocation(&signed, &facts, &transport, NOW)
            .unwrap();
        let peer = TransportEvidence::VerifiedPeer {
            origin: invocation.presenter.clone(),
        };
        let forwarded = f
            .state
            .verify_invocation(&signed, &facts, &peer, NOW)
            .unwrap();
        assert_eq!(direct.signed_invocation(), forwarded.signed_invocation());
        assert_eq!(forwarded.actor(), &Actor::Account(f.account.clone()));
        assert_eq!(forwarded.body_hash(), &invocation.body_hash);
        assert!(forwarded.dependencies().len() > invocation.expected_policies.len());
        let wrong = TransportEvidence::GatewayNoise {
            device: invocation.device.clone(),
            gateway: f.audience.device.clone(),
        };
        assert!(
            f.state
                .verify_invocation(&signed, &facts, &wrong, NOW)
                .is_err()
        );
        let old_direct = Signed::sign(f.invocation(), &f.user).unwrap();
        assert!(
            f.state
                .verify_invocation(&old_direct, &facts, &peer, NOW)
                .is_err()
        );
        let gateway_account = AccountId::from_root(&PublicKey::of(&gateway_root));
        let mut document = f.state.account(&gateway_account).unwrap().clone();
        document.previous = f
            .state
            .head(&PolicyDomain::Account(gateway_account))
            .cloned();
        document.authority_epoch += 1;
        document.sequence = 0;
        document.authority = AuthorityBinding::from_key(PublicKey::of(&key(26)));
        document
            .devices
            .retain(|h| f.state.device(h).unwrap().payload.device != invocation.presenter);
        document.revoked_devices.push(invocation.presenter);
        f.state
            .install_account(Signed::sign(document, &gateway_root).unwrap())
            .unwrap();
        assert!(
            f.state
                .verify_invocation(&signed, &facts, &peer, NOW)
                .is_err()
        );
    }

    #[test]
    fn sdk_adapter_and_execution_sessions_are_verified_independently() {
        let mut f = Fixture::new();
        let child = f.delegate();
        let mut invocation = f.service_invocation(&child);
        let sdk = SessionRef {
            target: f.audience.device.clone(),
            session_id: "manager-sdk".into(),
        };
        let execution = SessionRef {
            target: f.audience.device.clone(),
            session_id: "executor-session".into(),
        };
        invocation.context.sdk_session = Some(sdk.clone());
        invocation.context.execution_session = Some(execution.clone());
        let transport = TransportEvidence::LocalAgentAdapter {
            service: f.audience.service.clone(),
            session: sdk.clone(),
        };
        let signed = Signed::sign(invocation.clone(), &f.node).unwrap();
        assert!(
            f.state
                .verify_invocation(&signed, &f.facts(&invocation), &transport, NOW)
                .is_ok()
        );
        let wrong_adapter = TransportEvidence::LocalAgentAdapter {
            service: f.audience.service.clone(),
            session: execution,
        };
        assert!(
            f.state
                .verify_invocation(&signed, &f.facts(&invocation), &wrong_adapter, NOW)
                .is_err()
        );
        let mut wrong_facts = f.facts(&invocation);
        wrong_facts
            .context
            .execution_session
            .as_mut()
            .unwrap()
            .session_id = "wrong-execution".into();
        assert!(
            f.state
                .verify_invocation(&signed, &wrong_facts, &transport, NOW)
                .is_err()
        );
        let mut constraints = invocation.context.clone();
        constraints.execution_session.as_mut().unwrap().session_id = "pinned-execution".into();
        assert!(!context_tighter(&constraints, &invocation.context));
        invocation
            .context
            .execution_session
            .as_mut()
            .unwrap()
            .session_id
            .clear();
        assert!(
            f.state
                .verify_invocation(
                    &Signed::sign(invocation.clone(), &f.node).unwrap(),
                    &f.facts(&invocation),
                    &transport,
                    NOW
                )
                .is_err()
        );
        let mut direct = f.invocation();
        direct.context.sdk_session = Some(sdk);
        assert!(f.check(direct, &f.user, NOW).is_err());
    }

    #[test]
    fn service_membership_checks_space_subject_and_live_revocation() {
        let mut f = Fixture::new();
        assert!(
            f.state
                .verify_service_device(&f.space, &f.audience.device, NOW)
                .is_ok()
        );
        assert!(
            f.state
                .verify_service_device(&f.space, &DeviceId::from_key(&PublicKey::of(&f.user)), NOW)
                .is_err()
        );
        assert!(
            f.state
                .verify_service_device(
                    &SpaceId::team(&PublicKey::of(&key(29))),
                    &f.audience.device,
                    NOW
                )
                .is_err()
        );
        let mut document = f.state.account(&f.account).unwrap().clone();
        document.previous = f
            .state
            .head(&PolicyDomain::Account(f.account.clone()))
            .cloned();
        document.sequence += 1;
        document
            .devices
            .retain(|h| f.state.device(h).unwrap().payload.device != f.audience.device);
        document.revoked_devices.push(f.audience.device.clone());
        f.state
            .install_account(Signed::sign(document, &f.root).unwrap())
            .unwrap();
        assert_eq!(
            f.state
                .verify_service_device(&f.space, &f.audience.device, NOW)
                .unwrap_err(),
            AuthorizationError::Revoked
        );
    }

    #[test]
    fn all_ancestors_must_remain_registered() {
        let mut f = Fixture::new();
        let child = f.delegate();
        let accepted = f.check(f.service_invocation(&child), &f.node, NOW).unwrap();
        let root = f.root_grant.clone();
        f.update_space(|d| d.grants.retain(|h| h != &root));
        assert!(matches!(
            f.state.check_current(&accepted, NOW),
            Err(AuthorizationError::NeedPolicySync(_))
        ));
        assert_eq!(
            f.check(f.service_invocation(&child), &f.node, NOW)
                .unwrap_err(),
            AuthorizationError::Revoked
        );
        let mut d = f.state.space(&f.space).unwrap().clone();
        d.previous = f.state.head(&PolicyDomain::Space(f.space.clone())).cloned();
        d.sequence += 1;
        d.grants.push(root);
        d.grants.sort();
        assert_eq!(
            f.state
                .install_space(Signed::sign(d, &f.root).unwrap())
                .unwrap_err(),
            AuthorizationError::InvalidProof("revoked grant cannot be reactivated")
        );
    }
    #[test]
    fn delegation_cannot_widen_or_splice_scope_actions_audience_and_time() {
        let mut f = Fixture::new();
        for field in 0..5 {
            let mut child = f.child();
            match field {
                0 => child.scope = ResourceScope::WholeSpace(f.space.clone()),
                1 => {
                    child.actions.push(Action::OperationResolve);
                    child.actions.sort();
                }
                2 => child.audience.device = DeviceId::from_key(&PublicKey::of(&key(20))),
                3 => child.expires_at_ms += 1,
                _ => child.remaining_delegations = 3,
            }
            assert_eq!(
                f.state
                    .install_grant(Signed::sign(child, &f.user).unwrap(), NOW)
                    .unwrap_err(),
                AuthorizationError::InvalidProof("delegation widens parent grant")
            );
        }
        let mut forged = f.child();
        forged.parent = None;
        assert_eq!(
            f.state
                .install_grant(Signed::sign(forged, &f.user).unwrap(), NOW)
                .unwrap_err(),
            AuthorizationError::InvalidProof("root grant requires space root")
        );
    }
    #[test]
    fn freshness_is_per_invocation_target_stage_and_current_head() {
        let f = Fixture::new();
        let auth = f.check(f.invocation(), &f.user, NOW).unwrap();
        let challenge = FreshnessChallenge::new(
            &auth,
            CheckStage::Admit,
            Nonce::from_bytes([10; 32]),
            NOW,
            NOW + 10_000,
        )
        .unwrap();
        let receipts = f.receipts(&auth, &challenge);
        let verified = f
            .state
            .verify_freshness(&auth, &challenge, &receipts, NOW)
            .unwrap();
        assert_eq!(verified.nonce(), &Nonce::from_bytes([10; 32]));
        assert_eq!(verified.stage(), CheckStage::Admit);
        let mut changed = challenge.clone();
        changed.stage = CheckStage::Start;
        assert_eq!(
            f.state
                .verify_freshness(&auth, &changed, &receipts, NOW)
                .unwrap_err(),
            AuthorizationError::InvalidProof("freshness receipt mismatch")
        );
        let mut changed = challenge.clone();
        changed.audience.device = DeviceId::from_key(&PublicKey::of(&key(20)));
        assert_eq!(
            f.state
                .verify_freshness(&auth, &changed, &receipts, NOW)
                .unwrap_err(),
            AuthorizationError::InvalidProof("freshness challenge mismatch")
        );
        let mut changed = receipts.clone();
        let old = f
            .state
            .account_history(&f.account)
            .unwrap()
            .first()
            .unwrap();
        changed[0].payload.current = account_stamp(old).unwrap();
        changed[0] = Signed::sign(changed[0].payload.clone(), &f.authority).unwrap();
        assert_eq!(
            f.state
                .verify_freshness(&auth, &challenge, &changed, NOW)
                .unwrap_err(),
            AuthorizationError::InvalidProof("freshness receipt mismatch")
        );
        let mut malformed = challenge.clone();
        malformed.expires_at_ms = 0;
        assert_eq!(
            f.state
                .verify_freshness(&auth, &malformed, &receipts, NOW)
                .unwrap_err(),
            AuthorizationError::InvalidProof("invalid validity interval")
        );
    }
    #[test]
    fn root_governance_cannot_be_replaced_by_authority_or_recovery_self_assignment() {
        let mut f = Fixture::new();
        let mut d = f.state.account(&f.account).unwrap().clone();
        d.previous = f
            .state
            .head(&PolicyDomain::Account(f.account.clone()))
            .cloned();
        d.sequence += 1;
        assert!(
            f.state
                .install_account(Signed::sign(d.clone(), &f.authority).unwrap())
                .is_err()
        );
        d.recovery = RecoveryPolicy {
            generation: 1,
            keys: vec![PublicKey::of(&key(21))],
            threshold: 1,
        };
        assert_eq!(
            f.state
                .install_account(Signed::sign(d.clone(), &f.root).unwrap())
                .unwrap_err(),
            AuthorizationError::InvalidProof("recovery threshold not met")
        );
        d.approve_recovery(&f.recovery).unwrap();
        f.state
            .install_account(Signed::sign(d, &f.root).unwrap())
            .unwrap();
        assert_eq!(f.state.account(&f.account).unwrap().recovery.generation, 1);
    }
    #[test]
    fn recovery_advances_epoch_and_requires_new_root_possession() {
        let mut f = Fixture::new();
        let next = key(22);
        let mut d = f.state.account(&f.account).unwrap().clone();
        d.previous = f
            .state
            .head(&PolicyDomain::Account(f.account.clone()))
            .cloned();
        d.authority_epoch = 1;
        d.sequence = 0;
        d.root_generation = 1;
        d.current_root = PublicKey::of(&next);
        d.approve_recovery(&f.recovery).unwrap();
        assert_eq!(
            f.state
                .install_account(Signed::sign(d.clone(), &f.recovery).unwrap())
                .unwrap_err(),
            AuthorizationError::InvalidProof("new root must prove possession")
        );
        d.accept_root(&next).unwrap();
        let stamp = f
            .state
            .install_account(Signed::sign(d, &f.recovery).unwrap())
            .unwrap();
        assert_eq!(stamp.authority_epoch, 1);
        assert_eq!(f.state.account(&f.account).unwrap().account, f.account);
        let old = f.state.account_history(&f.account).unwrap()[1].clone();
        assert_eq!(
            f.state.install_account(old).unwrap_err(),
            AuthorizationError::PolicyConflict
        );
    }

    #[test]
    fn grants_naturally_expire_before_invocation_and_cannot_refresh() {
        let mut f = Fixture::new();
        let mut grant = f.state.grant(&f.root_grant).unwrap().payload.clone();
        grant.expires_at_ms = NOW + 5;
        grant.nonce = Nonce::from_bytes([40; 32]);
        let hash = f
            .state
            .install_grant(Signed::sign(grant, &f.root).unwrap(), NOW)
            .unwrap();
        f.update_space(|d| d.grants.push(hash.clone()));
        let mut i = f.invocation();
        i.grant_leaf = hash;
        let auth = f.check(i, &f.user, NOW).unwrap();
        assert_eq!(auth.expires_at_ms(), NOW + 5);
        assert_eq!(
            f.state.check_current(&auth, NOW + 5).unwrap_err(),
            AuthorizationError::Expired
        );
    }
    #[test]
    fn account_recovery_invalidates_old_personal_space_governance() {
        let mut f = Fixture::new();
        let next = key(22);
        let mut d = f.state.account(&f.account).unwrap().clone();
        d.previous = f
            .state
            .head(&PolicyDomain::Account(f.account.clone()))
            .cloned();
        d.authority_epoch = 1;
        d.sequence = 0;
        d.root_generation = 1;
        d.current_root = PublicKey::of(&next);
        d.approve_recovery(&f.recovery).unwrap();
        d.accept_root(&next).unwrap();
        f.state
            .install_account(Signed::sign(d, &f.recovery).unwrap())
            .unwrap();
        let mut space = f.state.space(&f.space).unwrap().clone();
        space.previous = f.state.head(&PolicyDomain::Space(f.space.clone())).cloned();
        space.sequence += 1;
        assert!(
            f.state
                .install_space(Signed::sign(space.clone(), &f.root).unwrap())
                .is_err()
        );
        space.current_root = PublicKey::of(&next);
        space.root_generation = 1;
        f.state
            .install_space(Signed::sign(space, &next).unwrap())
            .unwrap();
        assert_eq!(
            f.state.space(&f.space).unwrap().current_root,
            PublicKey::of(&next)
        );
    }
    #[test]
    fn signature_budget_counts_issuer_and_possession_and_caches_duplicates() {
        let f = Fixture::new();
        let source = f
            .state
            .device(
                f.state
                    .account(&f.account)
                    .unwrap()
                    .devices
                    .first()
                    .unwrap(),
            )
            .unwrap();
        let mut budget = Budget::default();
        for index in 0..16 {
            let identity = key(40 + index);
            let mut c = source.payload.clone();
            c.public_key = PublicKey::of(&identity);
            c.device = DeviceId::from_key(&c.public_key);
            c.nonce = Nonce::from_bytes([index; 32]);
            c.prove_possession(&identity).unwrap();
            let signed = Signed::sign(c, &f.root).unwrap();
            budget.verify(&signed).unwrap();
            budget
                .verify_raw(
                    &signed.payload.public_key,
                    "device-possession-v2",
                    &signed.payload.possession_bytes().unwrap(),
                    signed.payload.possession.as_ref().unwrap(),
                )
                .unwrap();
            budget
                .verify_raw(
                    &signed.payload.public_key,
                    "device-possession-v2",
                    &signed.payload.possession_bytes().unwrap(),
                    signed.payload.possession.as_ref().unwrap(),
                )
                .unwrap();
        }
        assert_eq!(budget.objects.len(), 16);
        assert_eq!(budget.signatures.len(), 32);
        let signed = Signed::sign(f.invocation(), &f.user).unwrap();
        assert_eq!(
            budget.verify(&signed).unwrap_err(),
            AuthorizationError::BudgetExceeded
        );
        assert_eq!(budget.signatures.len(), 32);
    }

    fn client_bundle(f: &Fixture) -> (ClientAuthorizationBundle, Vec<PolicyPin>) {
        let ah = f.state.account_history(&f.account).unwrap();
        let sh = f.state.space_history(&f.space).unwrap();
        let mut journal = vec![JournalEntry::AccountGenesis(ah[0].clone())];
        for device in f.state.devices.values() {
            journal.push(JournalEntry::Device(device.clone()));
        }
        for d in &ah[1..] {
            journal.push(JournalEntry::Account(d.clone()));
        }
        journal.push(JournalEntry::SpaceGenesis(sh[0].clone()));
        journal.push(JournalEntry::Resource(
            f.state
                .resource(&f.space, &f.resource.resource)
                .unwrap()
                .clone(),
        ));
        journal.push(JournalEntry::Grant(
            f.state.grant(&f.root_grant).unwrap().clone(),
        ));
        for d in &sh[1..] {
            journal.push(JournalEntry::Space(d.clone()));
        }
        let pins = vec![
            PolicyPin {
                domain: PolicyDomain::Account(f.account.clone()),
                genesis_root: PublicKey::of(&f.root),
                minimum_head: f
                    .state
                    .head(&PolicyDomain::Account(f.account.clone()))
                    .cloned(),
            },
            PolicyPin {
                domain: PolicyDomain::Space(f.space.clone()),
                genesis_root: PublicKey::of(&f.root),
                minimum_head: f.state.head(&PolicyDomain::Space(f.space.clone())).cloned(),
            },
        ];
        (
            ClientAuthorizationBundle {
                journal,
                grant_leaf: f.root_grant.clone(),
            },
            pins,
        )
    }
    #[test]
    fn scoped_resource_bundle_hides_unrelated_records_and_rejects_missing_or_tampered_proofs() {
        let mut f = Fixture::new();
        let hidden_resource = ResourceId::from_bytes([91; 32]);
        let hidden_host = ServiceId::from_key(&PublicKey::of(&key(92)));
        let mut record = f
            .state
            .resource(&f.space, &f.resource.resource)
            .unwrap()
            .payload
            .clone();
        record.resource = hidden_resource.clone();
        record.host = hidden_host.clone();
        record.nonce = Nonce::from_bytes([91; 32]);
        let hidden_hash = f
            .state
            .install_resource(Signed::sign(record, &f.root).unwrap())
            .unwrap();
        let mut grant = f.state.grant(&f.root_grant).unwrap().payload.clone();
        grant.scope = ResourceScope::Exact(ResourceRef {
            space: f.space.clone(),
            resource: hidden_resource.clone(),
        });
        grant.audience.service = hidden_host.clone();
        grant.nonce = Nonce::from_bytes([92; 32]);
        let hidden_grant = f
            .state
            .install_grant(Signed::sign(grant, &f.root).unwrap(), NOW)
            .unwrap();
        f.update_space(|d| {
            d.resources.push(hidden_hash.clone());
            d.grants.push(hidden_grant.clone());
        });
        let (bundle, pins) = client_bundle(&f);
        let json = serde_json::to_string(&bundle).unwrap();
        assert!(!json.contains(hidden_resource.as_str()));
        assert!(!json.contains(hidden_host.as_str()));
        assert!(json.contains(hidden_hash.as_str()));
        assert!(json.contains(hidden_grant.as_str()));
        let verify = |bundle: &ClientAuthorizationBundle| {
            verify_client_authorization(
                &serde_json::to_string(bundle).unwrap(),
                &serde_json::to_string(&pins).unwrap(),
                f.account.as_str(),
                f.space.as_str(),
                PublicKey::of(&f.user).as_str(),
                NOW as f64,
            )
        };
        let verified = verify(&bundle).unwrap();
        assert_eq!(verified.resources().len(), 1);
        assert_eq!(
            verified.resources()[0].payload.resource,
            f.resource.resource
        );
        let policy: serde_json::Value =
            serde_json::from_str(&verified.policy_json().unwrap()).unwrap();
        assert_eq!(
            policy["resources"][0]["payload"]["resource"],
            f.resource.resource.as_str()
        );
        assert!(
            !verified
                .policy_json()
                .unwrap()
                .contains(hidden_resource.as_str())
        );
        let mut missing = bundle.clone();
        missing
            .journal
            .retain(|e| !matches!(e, JournalEntry::Resource(_)));
        assert!(verify(&missing).is_err());
        let mut tampered = bundle.clone();
        for entry in &mut tampered.journal {
            if let JournalEntry::Resource(signed) = entry {
                signed.payload.host = hidden_host.clone();
            }
        }
        assert!(verify(&tampered).is_err());
        let mut cross_space = bundle;
        for entry in &mut cross_space.journal {
            if let JournalEntry::Resource(signed) = entry {
                signed.payload.space = SpaceId::team(&PublicKey::of(&key(93)));
                *signed = Signed::sign(signed.payload.clone(), &f.root).unwrap();
            }
        }
        assert!(verify(&cross_space).is_err());
    }

    #[test]
    fn conflicting_resources_are_rejected_before_and_after_policy_registration() {
        let mut f = Fixture::new();
        let mut record = f
            .state
            .resource(&f.space, &f.resource.resource)
            .unwrap()
            .payload
            .clone();
        record.nonce = Nonce::from_bytes([94; 32]);
        let signed = Signed::sign(record, &f.root).unwrap();
        let hash = f.state.install_resource(signed.clone()).unwrap();
        let mut document = f.state.space(&f.space).unwrap().clone();
        document.previous = f.state.head(&PolicyDomain::Space(f.space.clone())).cloned();
        document.sequence += 1;
        document.resources.push(hash.clone());
        document.resources.sort();
        let proposal = Signed::sign(document.clone(), &f.root).unwrap();
        assert_eq!(
            f.state.install_space(proposal.clone()).unwrap_err(),
            AuthorizationError::InvalidProof("invalid registered resource")
        );
        f.state.resources.remove(&hash);
        f.state.install_space(proposal).unwrap();
        assert_eq!(
            f.state.install_resource(signed).unwrap_err(),
            AuthorizationError::InvalidProof("conflicting registered resource")
        );
    }

    #[test]
    fn expired_membership_does_not_hide_an_independent_current_membership() {
        let mut f = Fixture::new();
        let issued_under = f
            .state
            .head(&PolicyDomain::Space(f.space.clone()))
            .unwrap()
            .clone();
        let member = Membership {
            version: AUTH_VERSION,
            space: f.space.clone(),
            subject: Actor::Account(f.account.clone()),
            roles: vec![SpaceRole::Operator],
            issued_under: issued_under.clone(),
            not_before_ms: 0,
            expires_at_ms: NOW + 1,
            nonce: Nonce::from_bytes([95; 32]),
        };
        let expired = f
            .state
            .install_membership(Signed::sign(member.clone(), &f.root).unwrap(), NOW)
            .unwrap();
        let mut current_member = member;
        current_member.expires_at_ms = 1_000_000;
        let current = f
            .state
            .install_membership(Signed::sign(current_member, &f.root).unwrap(), NOW)
            .unwrap();
        // Choose the expired record first in canonical hash order to exercise the failing branch.
        let (expired, current) = if expired < current {
            (expired, current)
        } else {
            let mut candidate = f.state.membership(&expired).unwrap().payload.clone();
            let mut found = None;
            for nonce in 0..=255 {
                candidate.nonce = Nonce::from_bytes([nonce; 32]);
                let signed = Signed::sign(candidate.clone(), &f.root).unwrap();
                if signed.hash().unwrap() < current {
                    found = Some(signed);
                    break;
                }
            }
            (
                f.state.install_membership(found.unwrap(), NOW).unwrap(),
                current,
            )
        };
        let mut grant = f.state.grant(&f.root_grant).unwrap().payload.clone();
        grant.grantee = Grantee::MembersOf(MembersOf {
            space: f.space.clone(),
            pinned_genesis: f.state.space_history(&f.space).unwrap()[0].hash().unwrap(),
            kind: MemberKind::Account,
            roles_any: vec![SpaceRole::Operator],
        });
        grant.nonce = Nonce::from_bytes([96; 32]);
        let grant_hash = f
            .state
            .install_grant(Signed::sign(grant, &f.root).unwrap(), NOW)
            .unwrap();
        f.update_space(|d| {
            d.memberships = vec![expired.clone(), current.clone()];
            d.grants.push(grant_hash.clone());
        });
        let mut invocation = f.invocation();
        invocation.grant_leaf = grant_hash.clone();
        assert!(f.check(invocation, &f.user, NOW + 2).is_ok());
        f.update_space(|d| d.memberships = vec![expired]);
        let mut invocation = f.invocation();
        invocation.grant_leaf = grant_hash;
        assert!(f.check(invocation, &f.user, NOW + 2).is_err());
    }

    #[test]
    fn client_scope_comes_from_pinned_policy_and_matches_native_wasm_vector() {
        let f = Fixture::new();
        let (bundle, pins) = client_bundle(&f);
        let bundle_json = serde_json::to_string(&bundle).unwrap();
        let pins_json = serde_json::to_string(&pins).unwrap();
        let verified = verify_client_authorization(
            &bundle_json,
            &pins_json,
            f.account.as_str(),
            f.space.as_str(),
            PublicKey::of(&f.user).as_str(),
            NOW as f64,
        )
        .unwrap();
        assert_eq!(
            verified.scope,
            ClientScope {
                account_id: f.account.clone(),
                space_id: f.space.clone(),
                device_id: DeviceId::from_key(&PublicKey::of(&f.user))
            }
        );
        assert!(
            verify_client_authorization(
                &bundle_json,
                "[]",
                f.account.as_str(),
                f.space.as_str(),
                PublicKey::of(&f.user).as_str(),
                NOW as f64
            )
            .is_err()
        );
        let mut tampered = bundle.clone();
        if let JournalEntry::Account(d) = &mut tampered.journal[3] {
            d.payload.devices.clear();
        } else {
            panic!("fixture order changed");
        }
        assert!(
            verify_client_authorization(
                &serde_json::to_string(&tampered).unwrap(),
                &pins_json,
                f.account.as_str(),
                f.space.as_str(),
                PublicKey::of(&f.user).as_str(),
                NOW as f64
            )
            .is_err()
        );
        if let Ok(path) = std::env::var("VIBEFACE_AUTH_VECTOR_PATH") {
            std::fs::write(path, serde_json::to_vec(&serde_json::json!({"bundle":bundle,"pins":pins,"account":f.account,"space":f.space,"public_key":PublicKey::of(&f.user),"now_ms":NOW,"scope":verified.scope_json().unwrap(),"policy":verified.policy_json().unwrap(),"invocation":f.invocation(),"signed_invocation":Signed::sign(f.invocation(),&f.user).unwrap()})).unwrap()).unwrap();
        }
    }
    #[test]
    fn revoked_authority_requires_higher_epoch_replacement() {
        let mut f = Fixture::new();
        let mut d = f.state.account(&f.account).unwrap().clone();
        d.previous = f
            .state
            .head(&PolicyDomain::Account(f.account.clone()))
            .cloned();
        d.sequence += 1;
        d.revoked_devices.push(d.authority.device.clone());
        assert_eq!(
            f.state
                .install_account(Signed::sign(d.clone(), &f.root).unwrap())
                .unwrap_err(),
            AuthorizationError::InvalidProof("revoked authority requires replacement")
        );
        d.authority = AuthorityBinding::from_key(PublicKey::of(&key(50)));
        d.authority_epoch = 1;
        d.sequence = 0;
        let stamp = f
            .state
            .install_account(Signed::sign(d, &f.root).unwrap())
            .unwrap();
        assert_eq!(stamp.authority_epoch, 1);
        let auth = f.check(f.invocation(), &f.user, NOW).unwrap();
        let challenge = FreshnessChallenge::new(
            &auth,
            CheckStage::Admit,
            Nonce::from_bytes([51; 32]),
            NOW,
            NOW + 10_000,
        )
        .unwrap();
        let old_receipts = f.receipts(&auth, &challenge);
        assert_eq!(
            f.state
                .verify_freshness(&auth, &challenge, &old_receipts, NOW)
                .unwrap_err(),
            AuthorizationError::InvalidProof("receipt is not from current authority")
        );
    }
    #[test]
    fn daily_grant_registration_needs_issuer_capability_and_current_authority_commit() {
        let mut f = Fixture::new();
        let child = f
            .state
            .install_grant(Signed::sign(f.child(), &f.user).unwrap(), NOW)
            .unwrap();
        let previous = f
            .state
            .head(&PolicyDomain::Space(f.space.clone()))
            .unwrap()
            .clone();
        let mut d = f.state.space(&f.space).unwrap().clone();
        d.previous = Some(previous.clone());
        d.sequence += 1;
        d.grants.push(child.clone());
        d.grants.sort();
        d.governance_grant = Some(f.root_grant.clone());
        let proposal = Signed::sign(d, &f.user).unwrap();
        assert_eq!(
            f.state.install_space(proposal.clone()).unwrap_err(),
            AuthorizationError::InvalidProof("daily governance requires authority commit")
        );
        let commit = Signed::sign(
            PolicyCommit {
                version: AUTH_VERSION,
                previous,
                proposal_hash: proposal.hash().unwrap(),
                accepted_at_ms: NOW,
                issued_at_ms: NOW,
                expires_at_ms: NOW + 10_000,
                nonce: Nonce::from_bytes([99; 32]),
                dependencies: f.policies(),
            },
            &f.authority,
        )
        .unwrap();
        f.state
            .install_space_committed(f.committed_space(proposal, commit), NOW)
            .unwrap();
        assert!(f.state.space(&f.space).unwrap().grants.contains(&child));
        assert!(f.check(f.service_invocation(&child), &f.node, NOW).is_ok());
        let previous = f
            .state
            .head(&PolicyDomain::Space(f.space.clone()))
            .unwrap()
            .clone();
        let mut evil = f.state.space(&f.space).unwrap().clone();
        evil.previous = Some(previous.clone());
        evil.sequence += 1;
        evil.current_root = PublicKey::of(&f.user);
        let proposal = Signed::sign(evil, &f.user).unwrap();
        let commit = Signed::sign(
            PolicyCommit {
                version: AUTH_VERSION,
                previous,
                proposal_hash: proposal.hash().unwrap(),
                accepted_at_ms: NOW,
                issued_at_ms: NOW,
                expires_at_ms: NOW + 10_000,
                nonce: Nonce::from_bytes([99; 32]),
                dependencies: f.policies(),
            },
            &f.authority,
        )
        .unwrap();
        assert!(
            f.state
                .install_space_committed(f.committed_space(proposal, commit), NOW)
                .is_err()
        );
    }
    #[test]
    fn grant_chain_hits_signature_budget_before_object_or_depth_limit() {
        let mut f = Fixture::new();
        let mut actors = vec![];
        for i in 0..8 {
            let root = key(60 + i);
            let recovery = key(90 + i);
            let authority = key(120 + i);
            let enroller = key(150 + i);
            let user = key(180 + i);
            let account = AccountId::from_root(&PublicKey::of(&root));
            let mut d = f.state.account(&f.account).unwrap().clone();
            d.account = account.clone();
            d.genesis_root = PublicKey::of(&root);
            d.current_root = PublicKey::of(&root);
            d.previous = None;
            d.sequence = 0;
            d.authority = AuthorityBinding::from_key(PublicKey::of(&authority));
            d.recovery.keys = vec![PublicKey::of(&recovery)];
            d.devices.clear();
            let genesis = f
                .state
                .trust_account_genesis(
                    &PublicKey::of(&root),
                    Signed::sign(d.clone(), &root).unwrap(),
                )
                .unwrap();
            let mut c = DeviceCertificate {
                version: AUTH_VERSION,
                owner: PolicyDomain::Account(account.clone()),
                subject: Actor::Account(account.clone()),
                device: DeviceId::from_key(&PublicKey::of(&enroller)),
                public_key: PublicKey::of(&enroller),
                uses: vec![
                    DeviceUse::Noise,
                    DeviceUse::AccountInvocation,
                    DeviceUse::EnrollDevice,
                ],
                possession: None,
                enrollment_grant: None,
                issued_under: genesis.clone(),
                nonce: Nonce::from_bytes([i; 32]),
                not_before_ms: 0,
                expires_at_ms: 1_000_000,
            };
            c.prove_possession(&enroller).unwrap();
            let enroll_hash = f
                .state
                .install_device(Signed::sign(c.clone(), &root).unwrap(), NOW)
                .unwrap();
            d.previous = Some(genesis);
            d.sequence = 1;
            d.devices = vec![enroll_hash.clone()];
            d.enrollment = vec![EnrollmentAuthority {
                device: c.device.clone(),
                rights: vec![AccountRight::EnrollDevice],
                expires_at_ms: 1_000_000,
            }];
            let head = f
                .state
                .install_account(Signed::sign(d.clone(), &root).unwrap())
                .unwrap();
            c.device = DeviceId::from_key(&PublicKey::of(&user));
            c.public_key = PublicKey::of(&user);
            c.uses = vec![DeviceUse::Noise, DeviceUse::AccountInvocation];
            c.issued_under = head.clone();
            c.prove_possession(&user).unwrap();
            let user_hash = f
                .state
                .install_device(Signed::sign(c, &enroller).unwrap(), NOW)
                .unwrap();
            d.previous = Some(head);
            d.sequence = 2;
            d.devices.push(user_hash);
            d.devices.sort();
            f.state
                .install_account(Signed::sign(d, &root).unwrap())
                .unwrap();
            actors.push((account, user));
        }
        let mut grant = f.state.grant(&f.root_grant).unwrap().payload.clone();
        grant.remaining_delegations = 8;
        grant.grantee = Grantee::Actor(Actor::Account(actors[0].0.clone()));
        grant.nonce = Nonce::from_bytes([80; 32]);
        let mut parent = f
            .state
            .install_grant(Signed::sign(grant.clone(), &f.root).unwrap(), NOW)
            .unwrap();
        f.update_space(|d| d.grants.push(parent.clone()));
        for index in 1..8 {
            grant.parent = Some(parent.clone());
            grant.remaining_delegations -= 1;
            grant.grantee = Grantee::Actor(Actor::Account(actors[index].0.clone()));
            grant.issued_under = f
                .state
                .head(&PolicyDomain::Space(f.space.clone()))
                .unwrap()
                .clone();
            grant.nonce = Nonce::from_bytes([80 + index as u8; 32]);
            let result = f.state.install_grant(
                Signed::sign(grant.clone(), &actors[index - 1].1).unwrap(),
                NOW,
            );
            if index == 7 {
                assert_eq!(result.unwrap_err(), AuthorizationError::BudgetExceeded);
            } else {
                parent = result.unwrap();
                f.update_space(|d| d.grants.push(parent.clone()));
            }
        }
    }

    #[test]
    fn daily_governance_requires_all_dependency_receipts_and_rejects_empty_changes() {
        let mut f = Fixture::new();
        let child = f
            .state
            .install_grant(Signed::sign(f.child(), &f.user).unwrap(), NOW)
            .unwrap();
        let previous = f
            .state
            .head(&PolicyDomain::Space(f.space.clone()))
            .unwrap()
            .clone();
        let mut d = f.state.space(&f.space).unwrap().clone();
        d.previous = Some(previous.clone());
        d.sequence += 1;
        d.grants.push(child);
        d.grants.sort();
        d.governance_grant = Some(f.root_grant.clone());
        let proposal = Signed::sign(d, &f.user).unwrap();
        let commit = Signed::sign(
            PolicyCommit {
                version: AUTH_VERSION,
                previous,
                proposal_hash: proposal.hash().unwrap(),
                accepted_at_ms: NOW,
                issued_at_ms: NOW,
                expires_at_ms: NOW + 10_000,
                nonce: Nonce::from_bytes([77; 32]),
                dependencies: f.policies(),
            },
            &f.authority,
        )
        .unwrap();
        let mut change = f.committed_space(proposal, commit);
        change.receipts.remove(0);
        assert!(matches!(
            f.state.install_space_committed(change, NOW),
            Err(AuthorizationError::NeedFreshness(_))
        ));
        let previous = f
            .state
            .head(&PolicyDomain::Space(f.space.clone()))
            .unwrap()
            .clone();
        let mut d = f.state.space(&f.space).unwrap().clone();
        d.previous = Some(previous.clone());
        d.sequence += 1;
        d.governance_grant = Some(f.root_grant.clone());
        let proposal = Signed::sign(d, &f.user).unwrap();
        let commit = Signed::sign(
            PolicyCommit {
                version: AUTH_VERSION,
                previous,
                proposal_hash: proposal.hash().unwrap(),
                accepted_at_ms: NOW,
                issued_at_ms: NOW,
                expires_at_ms: NOW + 10_000,
                nonce: Nonce::from_bytes([77; 32]),
                dependencies: f.policies(),
            },
            &f.authority,
        )
        .unwrap();
        assert_eq!(
            f.state
                .install_space_committed(f.committed_space(proposal, commit), NOW)
                .unwrap_err(),
            AuthorizationError::InvalidProof("empty space governance change")
        );
    }
    #[test]
    fn team_service_enrollment_uses_account_governor_without_team_root_online() {
        let mut f = Fixture::new();
        let team_root = key(70);
        let node = key(71);
        let team = SpaceId::team(&PublicKey::of(&team_root));
        let mut d = f.state.space(&f.space).unwrap().clone();
        d.space = team.clone();
        d.root = SpaceRoot::Team {
            genesis_root: PublicKey::of(&team_root),
        };
        d.current_root = PublicKey::of(&team_root);
        d.previous = None;
        d.sequence = 0;
        d.grants.clear();
        d.resources.clear();
        let genesis = f
            .state
            .trust_space_genesis(
                &PublicKey::of(&team_root),
                Signed::sign(d.clone(), &team_root).unwrap(),
            )
            .unwrap();
        let grant = Grant {
            version: AUTH_VERSION,
            owner_space: team.clone(),
            scope: ResourceScope::WholeSpace(team.clone()),
            grantee: Grantee::Actor(Actor::Account(f.account.clone())),
            actions: vec![Action::ServiceEnroll],
            audience: Audience {
                service: d.authority.service.clone(),
                device: d.authority.device.clone(),
            },
            constraints: GrantConstraints::default(),
            parent: None,
            remaining_delegations: 0,
            issued_under: genesis.clone(),
            not_before_ms: 0,
            expires_at_ms: 1_000_000,
            nonce: Nonce::from_bytes([70; 32]),
        };
        let hash = f
            .state
            .install_grant(Signed::sign(grant, &team_root).unwrap(), NOW)
            .unwrap();
        d.previous = Some(genesis);
        d.sequence = 1;
        d.grants = vec![hash.clone()];
        let head = f
            .state
            .install_space(Signed::sign(d.clone(), &team_root).unwrap())
            .unwrap();
        let mut certificate = DeviceCertificate {
            version: AUTH_VERSION,
            owner: PolicyDomain::Space(team.clone()),
            subject: Actor::Service(ServiceId::from_key(&PublicKey::of(&node))),
            device: DeviceId::from_key(&PublicKey::of(&node)),
            public_key: PublicKey::of(&node),
            uses: vec![DeviceUse::Noise, DeviceUse::ServiceInvocation],
            possession: None,
            enrollment_grant: Some(hash.clone()),
            issued_under: head.clone(),
            nonce: Nonce::from_bytes([71; 32]),
            not_before_ms: 0,
            expires_at_ms: 1_000_000,
        };
        certificate.prove_possession(&node).unwrap();
        let certificate_hash = f
            .state
            .install_device(Signed::sign(certificate, &f.user).unwrap(), NOW)
            .unwrap();
        d.previous = Some(head.clone());
        d.sequence = 2;
        d.devices = vec![certificate_hash];
        d.governance_grant = Some(hash);
        let proposal = Signed::sign(d, &f.user).unwrap();
        let dependencies = vec![
            f.state
                .head(&PolicyDomain::Account(f.account.clone()))
                .unwrap()
                .clone(),
            head.clone(),
        ];
        let commit = Signed::sign(
            PolicyCommit {
                version: AUTH_VERSION,
                previous: head,
                proposal_hash: proposal.hash().unwrap(),
                accepted_at_ms: NOW,
                issued_at_ms: NOW,
                expires_at_ms: NOW + 10_000,
                nonce: Nonce::from_bytes([72; 32]),
                dependencies,
            },
            &f.authority,
        )
        .unwrap();
        f.state
            .install_space_committed(f.committed_space(proposal, commit), NOW)
            .unwrap();
        assert!(
            f.state
                .verify_service_device(&team, &DeviceId::from_key(&PublicKey::of(&node)), NOW)
                .is_ok()
        );
        assert!(
            f.state
                .verify_service_device(&team, &f.audience.device, NOW)
                .is_err()
        );
        let mut document = f.state.space(&team).unwrap().clone();
        let previous = f
            .state
            .head(&PolicyDomain::Space(team.clone()))
            .unwrap()
            .clone();
        let audience = Audience {
            service: ServiceId::from_key(&PublicKey::of(&node)),
            device: DeviceId::from_key(&PublicKey::of(&node)),
        };
        let resource = ResourceRef {
            space: team.clone(),
            resource: f.resource.resource.clone(),
        };
        let grant = Grant {
            version: AUTH_VERSION,
            owner_space: team.clone(),
            scope: ResourceScope::Exact(resource.clone()),
            grantee: Grantee::Actor(Actor::Service(audience.service.clone())),
            actions: vec![Action::MessageAppend],
            audience: audience.clone(),
            constraints: GrantConstraints::default(),
            parent: None,
            remaining_delegations: 0,
            issued_under: previous.clone(),
            not_before_ms: 0,
            expires_at_ms: 1_000_000,
            nonce: Nonce::from_bytes([73; 32]),
        };
        let leaf = f
            .state
            .install_grant(Signed::sign(grant, &team_root).unwrap(), NOW)
            .unwrap();
        document.previous = Some(previous.clone());
        document.sequence += 1;
        document.grants.push(leaf.clone());
        document.grants.sort();
        let resource_hash = f
            .state
            .install_resource(
                Signed::sign(
                    ResourceRecord {
                        version: AUTH_VERSION,
                        space: team.clone(),
                        resource: resource.resource.clone(),
                        kind: ResourceKind::Project,
                        host: audience.service.clone(),
                        issued_under: previous.clone(),
                        nonce: Nonce::from_bytes([74; 32]),
                    },
                    &team_root,
                )
                .unwrap(),
            )
            .unwrap();
        document.resources = vec![resource_hash];
        document.governance_grant = None;
        let head = f
            .state
            .install_space(Signed::sign(document, &team_root).unwrap())
            .unwrap();
        let mut invocation = f.invocation();
        invocation.actor = Actor::Service(audience.service.clone());
        invocation.initiator = invocation.actor.clone();
        invocation.device = audience.device.clone();
        invocation.presenter = audience.device.clone();
        invocation.audience = audience;
        invocation.resource = resource;
        invocation.grant_leaf = leaf;
        invocation.expected_policies = vec![head.clone()];
        assert!(
            matches!(f.check(invocation.clone(),&node,NOW),Err(AuthorizationError::NeedPolicySync(ref stamps)) if stamps.len()==2 && stamps[0].domain==PolicyDomain::Account(f.account.clone()))
        );
        invocation.expected_policies = vec![
            f.state
                .head(&PolicyDomain::Account(f.account.clone()))
                .unwrap()
                .clone(),
            head,
        ];
        assert_eq!(
            f.check(invocation, &node, NOW)
                .unwrap()
                .dependencies()
                .len(),
            2
        );
        assert_eq!(
            f.state
                .find_device(&DeviceId::from_key(&PublicKey::of(&node)))
                .unwrap()
                .payload
                .owner,
            PolicyDomain::Space(team)
        );
    }
    #[test]
    fn prepared_signing_matches_native_typed_signature() {
        let f = Fixture::new();
        let invocation = f.invocation();
        let json = serde_json::to_string(&invocation).unwrap();
        let typed = Signed::sign(invocation, &f.user).unwrap();
        let prepared: serde_json::Value = serde_json::from_str(
            &prepare_authorization_signing("invocation", &json, PublicKey::of(&f.user).as_str())
                .unwrap(),
        )
        .unwrap();
        assert_eq!(
            prepared["object_hash"].as_str().unwrap(),
            typed.hash().unwrap().as_str()
        );
        assert_eq!(
            f.user
                .sign(
                    prepared["domain"].as_str().unwrap(),
                    prepared["signing_bytes"].as_str().unwrap().as_bytes()
                )
                .unwrap(),
            typed.signature.as_str()
        );
        assert_eq!(
            serde_json::from_str::<Signed<Invocation>>(&sign_invocation(&json, &f.user).unwrap())
                .unwrap(),
            typed
        );
        let (bundle, pins) = client_bundle(&f);
        let auth = verify_client_authorization(
            &serde_json::to_string(&bundle).unwrap(),
            &serde_json::to_string(&pins).unwrap(),
            f.account.as_str(),
            f.space.as_str(),
            PublicKey::of(&f.user).as_str(),
            NOW as f64,
        )
        .unwrap();
        let body = r#"{"text":"hello"}"#;
        assert_eq!(
            derive_authorization_body_hash(body).unwrap(),
            typed.payload.body_hash.as_str()
        );
        assert_eq!(
            serde_json::from_str::<Signed<Invocation>>(
                &sign_client_invocation(
                    &auth,
                    &json,
                    body,
                    "message_append",
                    "op_1",
                    &f.user,
                    NOW as f64
                )
                .unwrap()
            )
            .unwrap(),
            typed
        );
        assert!(
            sign_client_invocation(
                &auth,
                &json,
                body,
                "task_accept",
                "op_1",
                &f.user,
                NOW as f64
            )
            .is_err()
        );
        assert!(
            sign_client_invocation(
                &auth,
                &json,
                body,
                "message_append",
                "op_2",
                &f.user,
                NOW as f64
            )
            .is_err()
        );
        assert!(
            sign_client_invocation(
                &auth,
                &json,
                r#"{"text":"changed"}"#,
                "message_append",
                "op_1",
                &f.user,
                NOW as f64
            )
            .is_err()
        );
    }

    #[test]
    fn enrolled_account_device_registers_another_device_with_fresh_policy_commit() {
        let mut f = Fixture::new();
        let user_device = DeviceId::from_key(&PublicKey::of(&f.user));
        let old = f.state.find_device(&user_device).unwrap().hash().unwrap();
        let mut certificate = f.state.find_device(&user_device).unwrap().payload.clone();
        certificate.uses.push(DeviceUse::EnrollDevice);
        certificate.uses.sort();
        certificate.prove_possession(&f.user).unwrap();
        let manager = f
            .state
            .install_device(Signed::sign(certificate, &f.root).unwrap(), NOW)
            .unwrap();
        let previous = f
            .state
            .head(&PolicyDomain::Account(f.account.clone()))
            .unwrap()
            .clone();
        let mut d = f.state.account(&f.account).unwrap().clone();
        d.previous = Some(previous);
        d.sequence += 1;
        d.devices.retain(|h| h != &old);
        d.devices.push(manager);
        d.devices.sort();
        d.enrollment = vec![EnrollmentAuthority {
            device: user_device,
            rights: vec![AccountRight::EnrollDevice, AccountRight::RevokeDevice],
            expires_at_ms: 1_000_000,
        }];
        let head = f
            .state
            .install_account(Signed::sign(d.clone(), &f.root).unwrap())
            .unwrap();
        let next = key(78);
        let mut c = DeviceCertificate {
            version: AUTH_VERSION,
            owner: PolicyDomain::Account(f.account.clone()),
            subject: Actor::Account(f.account.clone()),
            device: DeviceId::from_key(&PublicKey::of(&next)),
            public_key: PublicKey::of(&next),
            uses: vec![DeviceUse::Noise, DeviceUse::AccountInvocation],
            possession: None,
            enrollment_grant: None,
            issued_under: head.clone(),
            nonce: Nonce::from_bytes([78; 32]),
            not_before_ms: 0,
            expires_at_ms: 1_000_000,
        };
        c.prove_possession(&next).unwrap();
        let hash = f
            .state
            .install_device(Signed::sign(c, &f.user).unwrap(), NOW)
            .unwrap();
        d.previous = Some(head.clone());
        d.sequence += 1;
        d.devices.push(hash);
        d.devices.sort();
        let proposal = Signed::sign(d, &f.user).unwrap();
        let commit = Signed::sign(
            PolicyCommit {
                version: AUTH_VERSION,
                previous: head.clone(),
                proposal_hash: proposal.hash().unwrap(),
                accepted_at_ms: NOW,
                issued_at_ms: NOW,
                expires_at_ms: NOW + 10_000,
                nonce: Nonce::from_bytes([79; 32]),
                dependencies: vec![head.clone()],
            },
            &f.authority,
        )
        .unwrap();
        let receipt = Signed::sign(
            FreshnessReceipt {
                version: AUTH_VERSION,
                current: head,
                challenge_hash: commit.challenge_hash().unwrap(),
                issued_at_ms: NOW,
                expires_at_ms: NOW + 10_000,
            },
            &f.authority,
        )
        .unwrap();
        f.state
            .install_account_committed(
                Committed {
                    proposal,
                    commit,
                    receipts: vec![receipt],
                },
                NOW,
            )
            .unwrap();
        assert_eq!(
            f.state
                .find_device(&DeviceId::from_key(&PublicKey::of(&next)))
                .unwrap()
                .payload
                .subject,
            Actor::Account(f.account)
        );
    }

    #[test]
    fn admitted_operation_renews_past_request_ttl_and_policy_updates_but_not_revocation() {
        let mut f = Fixture::new();
        let invocation = f.invocation();
        let facts = f.facts(&invocation);
        let transport = TransportEvidence::DirectNoise {
            device: invocation.device.clone(),
        };
        let auth = f.check(invocation, &f.user, NOW).unwrap();
        let challenge = FreshnessChallenge::new(
            &auth,
            CheckStage::Admit,
            Nonce::from_bytes([80; 32]),
            NOW,
            NOW + 10_000,
        )
        .unwrap();
        let receipts = f.receipts(&auth, &challenge);
        let fresh = f
            .state
            .verify_freshness(&auth, &challenge, &receipts, NOW)
            .unwrap();
        let later_fresh = f
            .state
            .verify_freshness(&auth, &challenge, &receipts, NOW + 1)
            .unwrap();
        assert!(f.state.admit_operation(&auth, &later_fresh, NOW).is_err());
        assert!(f.state.check_current(&auth, NOW - 1).is_err());
        let admission = f.state.admit_operation(&auth, &fresh, NOW).unwrap();
        assert!(
            FreshnessChallenge::new(
                &auth,
                CheckStage::Renew,
                Nonce::from_bytes([81; 32]),
                NOW,
                NOW + 10_000
            )
            .is_err()
        );
        f.update_space(|_| {});
        let now = 200_000;
        assert_eq!(
            f.state.check_current(&auth, now).unwrap_err(),
            AuthorizationError::Expired
        );
        let renewed = f
            .state
            .verify_operation_authorization(&admission, &facts, &transport, now)
            .unwrap();
        assert_eq!(renewed.signed_invocation(), admission.signed_invocation());
        assert!(renewed.expires_at_ms() > now);
        let challenge = FreshnessChallenge::new(
            &renewed,
            CheckStage::Renew,
            Nonce::from_bytes([82; 32]),
            now,
            now + 10_000,
        )
        .unwrap();
        let receipts: Vec<_> = renewed
            .dependencies()
            .iter()
            .map(|current| {
                Signed::sign(
                    FreshnessReceipt {
                        version: AUTH_VERSION,
                        current: current.clone(),
                        challenge_hash: challenge.hash().unwrap(),
                        issued_at_ms: now,
                        expires_at_ms: now + 10_000,
                    },
                    &f.authority,
                )
                .unwrap()
            })
            .collect();
        assert_eq!(
            f.state
                .verify_freshness(&renewed, &challenge, &receipts, now)
                .unwrap()
                .stage(),
            CheckStage::Renew
        );
        f.update_space(|d| d.grants.clear());
        assert_eq!(
            f.state
                .verify_operation_authorization(&admission, &facts, &transport, now)
                .unwrap_err(),
            AuthorizationError::Revoked
        );
    }

    #[test]
    fn typed_json_rejects_uppercase_duplicate_unknown_and_unsafe_numbers() {
        let f = Fixture::new();
        let signed = Signed::sign(f.invocation(), &f.user).unwrap();
        assert!(DeviceId::parse(&format!("dev2_{}", "AA".repeat(32))).is_err());
        assert_eq!(
            node_fingerprint(&f.node.public_key_hex()).unwrap(),
            f.node.node_id()
        );
        assert!(node_fingerprint(&"AA".repeat(32)).is_err());
        assert!(node_fingerprint("bad-key").is_err());
        let bytes = serde_json::to_vec(&signed).unwrap();
        assert_eq!(decode_signed::<Invocation>(&bytes).unwrap(), signed);
        let text = String::from_utf8(bytes).unwrap();
        let duplicate = text.replacen("\"version\":2", "\"version\":2,\"version\":2", 1);
        assert!(decode_signed::<Invocation>(duplicate.as_bytes()).is_err());
        let unknown = text.replacen("\"version\":2", "\"version\":2,\"human\":true", 1);
        assert!(decode_signed::<Invocation>(unknown.as_bytes()).is_err());
        let unsafe_number = text.replacen(
            "\"expires_at_ms\":90000",
            "\"expires_at_ms\":9007199254740992",
            1,
        );
        assert!(decode_signed::<Invocation>(unsafe_number.as_bytes()).is_err());
        assert!(decode_signed::<Invocation>(&vec![b' '; MAX_PROOF_BYTES + 1]).is_err());
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PolicyCommit {
    pub version: u16,
    pub previous: PolicyStamp,
    pub proposal_hash: ObjectHash,
    pub accepted_at_ms: u64,
    pub issued_at_ms: u64,
    pub expires_at_ms: u64,
    pub nonce: Nonce,
    pub dependencies: Vec<PolicyStamp>,
}
signed_object!(PolicyCommit, "policy-commit-v2");
impl Signed<PolicyCommit> {
    pub fn challenge_hash(&self) -> Result<ObjectHash> {
        let mut payload = self.payload.clone();
        payload.accepted_at_ms = 0;
        Ok(ObjectHash(format!(
            "h2_{}",
            digest(
                "policy-commit-challenge-v2",
                &canonical(&Unsigned {
                    issuer: &self.issuer,
                    payload: &payload
                })?
            )
        )))
    }
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Committed<T> {
    pub proposal: Signed<T>,
    pub commit: Signed<PolicyCommit>,
    pub receipts: Vec<Signed<FreshnessReceipt>>,
}

impl VerifiedPolicySet {
    pub fn policy_commit(&self, proposal_hash: &ObjectHash) -> Option<&Signed<PolicyCommit>> {
        self.commits.get(proposal_hash)
    }
    fn verify_commit<T: SignedObject>(
        &self,
        change: &Committed<T>,
        previous: &PolicyStamp,
        now_ms: u64,
    ) -> Result<Budget> {
        require(
            change.commit.payload.version == AUTH_VERSION
                && change.commit.payload.previous == *previous
                && change.commit.payload.proposal_hash == change.proposal.hash()?,
            "policy commit does not bind proposal",
        )?;
        let authority = match &previous.domain {
            PolicyDomain::Account(id) => self.account(id).map(|d| &d.authority),
            PolicyDomain::Space(id) => self.space(id).map(|d| &d.authority),
        }
        .ok_or(AuthorizationError::PolicyConflict)?;
        require(
            change.commit.issuer == authority.public_key,
            "policy commit is not from current authority",
        )?;
        validate_window(
            change.commit.payload.issued_at_ms,
            change.commit.payload.expires_at_ms,
            now_ms,
        )?;
        require(
            change.commit.payload.expires_at_ms - change.commit.payload.issued_at_ms
                <= EXECUTION_LEASE_MS,
            "policy challenge lifetime too long",
        )?;
        let accepted = change.commit.payload.accepted_at_ms;
        require(
            accepted <= now_ms && now_ms - accepted <= INVOCATION_MAX_AGE_MS,
            "policy commit is not fresh",
        )?;
        let mut budget = Budget::default();
        change.commit.verify_with_budget(&mut budget)?;
        change.proposal.verify_with_budget(&mut budget)?;
        Ok(budget)
    }
    pub fn install_account_committed(
        &mut self,
        change: Committed<AccountDocument>,
        now_ms: u64,
    ) -> Result<PolicyStamp> {
        let previous = self
            .head(&PolicyDomain::Account(
                change.proposal.payload.account.clone(),
            ))
            .ok_or(AuthorizationError::PolicyConflict)?;
        let budget = self.verify_commit(&change, previous, now_ms)?;
        let hash = change.proposal.hash()?;
        let stamp = self.apply_account(
            change.proposal,
            Some(now_ms),
            budget,
            Some((&change.commit, &change.receipts, now_ms)),
        )?;
        self.commits.insert(hash, change.commit);
        Ok(stamp)
    }
    pub fn install_space_committed(
        &mut self,
        change: Committed<SpaceDocument>,
        now_ms: u64,
    ) -> Result<PolicyStamp> {
        let previous = self
            .head(&PolicyDomain::Space(change.proposal.payload.space.clone()))
            .ok_or(AuthorizationError::PolicyConflict)?;
        let budget = self.verify_commit(&change, previous, now_ms)?;
        let hash = change.proposal.hash()?;
        let stamp = self.apply_space(
            change.proposal,
            Some(now_ms),
            budget,
            Some((&change.commit, &change.receipts, now_ms)),
        )?;
        self.commits.insert(hash, change.commit);
        Ok(stamp)
    }
    /// Reconstruct signed historical policy. Remote writes still require current freshness.
    pub fn replay_account_committed(
        &mut self,
        change: Committed<AccountDocument>,
    ) -> Result<PolicyStamp> {
        let at = change.commit.payload.accepted_at_ms;
        self.install_account_committed(change, at)
    }
    /// Reconstruct signed historical policy. Remote writes still require current freshness.
    pub fn replay_space_committed(
        &mut self,
        change: Committed<SpaceDocument>,
    ) -> Result<PolicyStamp> {
        let at = change.commit.payload.accepted_at_ms;
        self.install_space_committed(change, at)
    }
    fn ensure_personal_current(&self, space: &SpaceDocument) -> Result<()> {
        if let SpaceRoot::Personal { owner, .. } = &space.root {
            let account = self
                .account(owner)
                .ok_or(AuthorizationError::PolicyConflict)?;
            require(
                space.current_root == account.current_root
                    && space.root_generation == account.root_generation,
                "personal space must follow current account root",
            )?;
        }
        Ok(())
    }
    fn authorize_account_delta(
        &self,
        signed: &Signed<AccountDocument>,
        previous: &AccountDocument,
        at: u64,
        budget: &mut Budget,
    ) -> Result<()> {
        let d = &signed.payload;
        if d.authenticators != previous.authenticators {
            return Err(AuthorizationError::UserPresenceRequired);
        }
        let device = DeviceId::from_key(&signed.issuer);
        let certificate = self.find_device(&device)?;
        require(
            certificate.payload.subject == Actor::Account(d.account.clone())
                && certificate.payload.public_key == signed.issuer,
            "account governor is not account device",
        )?;
        self.validate_certificate(
            certificate,
            at,
            budget,
            &mut BTreeMap::new(),
            &mut BTreeSet::new(),
        )?;
        let authority = previous
            .enrollment
            .iter()
            .find(|e| e.device == device && at < e.expires_at_ms)
            .ok_or(AuthorizationError::Revoked)?;
        let additions: Vec<_> = d
            .devices
            .iter()
            .filter(|h| !previous.devices.contains(h))
            .collect();
        let removals = previous.devices.iter().any(|h| !d.devices.contains(h))
            || d.revoked_devices != previous.revoked_devices;
        require(
            additions.is_empty() || authority.rights.contains(&AccountRight::EnrollDevice),
            "governor lacks device enrollment right",
        )?;
        require(
            !removals || authority.rights.contains(&AccountRight::RevokeDevice),
            "governor lacks device revocation right",
        )?;
        for hash in additions {
            let certificate = self
                .devices
                .get(hash)
                .ok_or(AuthorizationError::InvalidProof(
                    "missing enrolled certificate",
                ))?;
            self.validate_certificate(
                certificate,
                at,
                budget,
                &mut BTreeMap::new(),
                &mut BTreeSet::new(),
            )?;
            require(
                self.devices
                    .get(hash)
                    .is_some_and(|c| c.issuer == signed.issuer),
                "governor cannot register another issuer certificate",
            )?;
        }
        let mut expected = previous.clone();
        expected.previous = d.previous.clone();
        expected.sequence = d.sequence;
        expected.devices = d.devices.clone();
        expected.revoked_devices = d.revoked_devices.clone();
        require(
            *d == expected,
            "account device changed root-controlled policy",
        )
    }
    fn governance_capability(
        &self,
        hash: &ObjectHash,
        signer: &PublicKey,
        space: &SpaceId,
        action: Option<Action>,
        resource: Option<&ResourceId>,
        at: u64,
        budget: &mut Budget,
    ) -> Result<()> {
        let path = self.grant_chain(Some(hash))?;
        let mut dependencies = BTreeMap::new();
        self.verify_grant_path(&path, at, budget, &mut dependencies, true)?;
        let leaf = &path
            .last()
            .ok_or(AuthorizationError::InvalidProof("missing governance grant"))?
            .payload;
        let actor = self.actor_for_signer(signer, at, budget, &mut dependencies)?;
        let authority = &self
            .space(space)
            .ok_or(AuthorizationError::PolicyConflict)?
            .authority;
        require(
            leaf.owner_space == *space
                && leaf.audience
                    == (Audience {
                        service: authority.service.clone(),
                        device: authority.device.clone(),
                    })
                && self.matches_grantee(&leaf.grantee, &actor, at, budget, &mut dependencies)?
                && action.is_none_or(|a| leaf.actions.contains(&a))
                && leaf.constraints.context == InvocationContext::default()
                && !leaf.constraints.require_webauthn_uv
                && (!leaf.constraints.require_account_actor || matches!(actor, Actor::Account(_))),
            "governance grant does not authorize change",
        )?;
        match resource {
            Some(resource) => require(
                scope_covers(
                    &leaf.scope,
                    &ResourceRef {
                        space: space.clone(),
                        resource: resource.clone(),
                    },
                ),
                "governance resource outside grant",
            ),
            None => require(
                leaf.scope == ResourceScope::WholeSpace(space.clone()),
                "governance requires whole-space scope",
            ),
        }
    }
    fn authorize_space_delta(
        &self,
        signed: &Signed<SpaceDocument>,
        previous: &SpaceDocument,
        at: u64,
        budget: &mut Budget,
    ) -> Result<()> {
        let d = &signed.payload;
        self.actor_for_signer(&signed.issuer, at, budget, &mut BTreeMap::new())?;
        require(
            d.grants != previous.grants
                || d.memberships != previous.memberships
                || d.devices != previous.devices
                || d.revoked_devices != previous.revoked_devices
                || d.resources != previous.resources,
            "empty space governance change",
        )?;

        let hash = d
            .governance_grant
            .as_ref()
            .ok_or(AuthorizationError::InvalidProof(
                "space change requires governance grant",
            ))?;
        let added: Vec<_> = d
            .grants
            .iter()
            .filter(|h| !previous.grants.contains(h))
            .collect();
        let removed: Vec<_> = previous
            .grants
            .iter()
            .filter(|h| !d.grants.contains(h))
            .collect();
        for grant_hash in added {
            let grant = self
                .grants
                .get(grant_hash)
                .ok_or(AuthorizationError::InvalidProof("missing proposed grant"))?;
            require(
                grant.issuer == signed.issuer && grant.payload.parent.as_ref() == Some(hash),
                "grant registration must use its own parent capability",
            )?;
            let path = self.grant_chain(Some(grant_hash))?;
            self.verify_grant_path(&path, at, budget, &mut BTreeMap::new(), false)?;
        }
        for grant_hash in removed {
            let grant = self
                .grants
                .get(grant_hash)
                .ok_or(AuthorizationError::PolicyConflict)?;
            let resource = match &grant.payload.scope {
                ResourceScope::WholeSpace(_) => None,
                ResourceScope::Exact(r) => Some(&r.resource),
            };
            self.governance_capability(
                hash,
                &signed.issuer,
                &d.space,
                Some(Action::GrantRevoke),
                resource,
                at,
                budget,
            )?;
        }
        if d.memberships != previous.memberships {
            for hash in d
                .memberships
                .iter()
                .filter(|h| !previous.memberships.contains(h))
            {
                require(
                    self.memberships
                        .get(hash)
                        .is_some_and(|m| m.issuer == signed.issuer),
                    "membership registration issuer mismatch",
                )?;
            }
            self.governance_capability(
                hash,
                &signed.issuer,
                &d.space,
                Some(Action::MemberManage),
                None,
                at,
                budget,
            )?;
        }
        if d.devices != previous.devices || d.revoked_devices != previous.revoked_devices {
            for hash in d.devices.iter().filter(|h| !previous.devices.contains(h)) {
                let certificate =
                    self.devices
                        .get(hash)
                        .ok_or(AuthorizationError::InvalidProof(
                            "missing service certificate",
                        ))?;
                self.validate_certificate(
                    certificate,
                    at,
                    budget,
                    &mut BTreeMap::new(),
                    &mut BTreeSet::new(),
                )?;
            }
            self.governance_capability(
                hash,
                &signed.issuer,
                &d.space,
                Some(Action::ServiceEnroll),
                None,
                at,
                budget,
            )?;
        }
        if d.resources != previous.resources {
            for resource_hash in d.resources.iter().chain(previous.resources.iter()) {
                if !d.resources.contains(resource_hash)
                    || !previous.resources.contains(resource_hash)
                {
                    let resource = self.resources.get(resource_hash).ok_or(
                        AuthorizationError::InvalidProof("missing changed resource record"),
                    )?;
                    budget.verify(resource)?;
                    if !previous.resources.contains(resource_hash) {
                        require(
                            resource.issuer == signed.issuer && resource.payload.space == d.space,
                            "resource registration issuer mismatch",
                        )?;
                    }
                    self.governance_capability(
                        hash,
                        &signed.issuer,
                        &d.space,
                        Some(Action::ResourceManage),
                        Some(&resource.payload.resource),
                        at,
                        budget,
                    )?;
                }
            }
        }
        let mut expected = previous.clone();
        expected.previous = d.previous.clone();
        expected.sequence = d.sequence;
        expected.governance_grant = d.governance_grant.clone();
        expected.grants = d.grants.clone();
        expected.memberships = d.memberships.clone();
        expected.devices = d.devices.clone();
        expected.revoked_devices = d.revoked_devices.clone();
        expected.resources = d.resources.clone();
        require(
            *d == expected,
            "space device changed root-controlled policy",
        )
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PolicyPin {
    pub domain: PolicyDomain,
    pub genesis_root: PublicKey,
    pub minimum_head: Option<PolicyStamp>,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(
    tag = "kind",
    content = "value",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum JournalEntry {
    AccountGenesis(Signed<AccountDocument>),
    Account(Signed<AccountDocument>),
    SpaceGenesis(Signed<SpaceDocument>),
    Space(Signed<SpaceDocument>),
    Device(Signed<DeviceCertificate>),
    Resource(Signed<ResourceRecord>),
    Authenticator(Signed<AuthenticatorRecord>),
    Membership(Signed<Membership>),
    Grant(Signed<Grant>),
    AccountCommit(Committed<AccountDocument>),
    SpaceCommit(Committed<SpaceDocument>),
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ClientAuthorizationBundle {
    pub journal: Vec<JournalEntry>,
    pub grant_leaf: ObjectHash,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ClientScope {
    pub account_id: AccountId,
    pub space_id: SpaceId,
    pub device_id: DeviceId,
}
#[derive(Clone, Debug)]
#[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen)]
pub struct VerifiedClientAuthorization {
    scope: ClientScope,
    heads: Vec<PolicyStamp>,
    grant_leaf: ObjectHash,
    actions: Vec<Action>,
    grant_scope: ResourceScope,
    audience: Audience,
    audience_public_key: PublicKey,
    constraints: GrantConstraints,
    account_enrollment_rights: Vec<AccountRight>,
    resources: Vec<Signed<ResourceRecord>>,
}
#[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen)]
impl VerifiedClientAuthorization {
    #[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen(js_name = scopeJson))]
    pub fn scope_json(&self) -> crate::Result<String> {
        serde_json::to_string(&self.scope).map_err(|_| "scope encoding failed".to_owned())
    }
    #[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen(js_name = policyJson))]
    pub fn policy_json(&self) -> crate::Result<String> {
        serde_json::to_string(
            &serde_json::json!({"heads": self.heads, "grant_leaf": self.grant_leaf,"actions": self.actions,"scope": self.grant_scope,"audience": self.audience,"audience_public_key": self.audience_public_key,"constraints": self.constraints,"account_enrollment_rights":self.account_enrollment_rights,"resources":self.resources}),
        )
        .map_err(|_| "policy encoding failed".to_owned())
    }
}
/// Pins come from the saved verified entry, not from this untrusted bundle.
#[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen(js_name = verifyClientAuthorization))]
pub fn verify_client_authorization(
    bundle_json: &str,
    pinned_roots_json: &str,
    expected_account: &str,
    expected_space: &str,
    expected_device_public_key: &str,
    now_ms: f64,
) -> crate::Result<VerifiedClientAuthorization> {
    fn verify(
        bundle_json: &str,
        pins_json: &str,
        account: &str,
        space: &str,
        key: &str,
        now_ms: f64,
    ) -> Result<VerifiedClientAuthorization> {
        require(
            bundle_json.len() <= 1_048_576
                && pins_json.len() <= MAX_PROOF_BYTES
                && now_ms.is_finite()
                && now_ms >= 0.0
                && now_ms.fract() == 0.0
                && now_ms <= 9_007_199_254_740_991.0,
            "invalid client verification input",
        )?;
        let bundle: ClientAuthorizationBundle = serde_json::from_str(bundle_json)
            .map_err(|_| AuthorizationError::InvalidProof("invalid client journal JSON"))?;
        let pins: Vec<PolicyPin> = serde_json::from_str(pins_json)
            .map_err(|_| AuthorizationError::InvalidProof("invalid policy pins JSON"))?;
        require(
            bundle.journal.len() <= 128
                && pins.len() <= MAX_POLICY_DOMAINS
                && pins.windows(2).all(|p| p[0].domain < p[1].domain),
            "invalid client journal bounds",
        )?;
        let now_ms = now_ms as u64;
        let account = AccountId::parse(account)?;
        let space = SpaceId::parse(space)?;
        let key = PublicKey::parse(key)?;
        let state = replay_policies(&pins, &bundle.journal)?;
        let certificate = state.find_device(&DeviceId::from_key(&key))?;
        require(
            certificate.payload.public_key == key
                && certificate.payload.subject == Actor::Account(account.clone()),
            "client device belongs to another account",
        )?;
        let mut budget = Budget::default();
        let mut dependencies = BTreeMap::new();
        state.validate_certificate(
            certificate,
            now_ms,
            &mut budget,
            &mut dependencies,
            &mut BTreeSet::new(),
        )?;
        let path = state.grant_chain(Some(&bundle.grant_leaf))?;
        state.verify_grant_path(&path, now_ms, &mut budget, &mut dependencies, true)?;
        let leaf = &path
            .last()
            .ok_or(AuthorizationError::InvalidProof("missing client grant"))?
            .payload;
        require(
            leaf.owner_space == space
                && state.matches_grantee(
                    &leaf.grantee,
                    &Actor::Account(account.clone()),
                    now_ms,
                    &mut budget,
                    &mut dependencies,
                )?,
            "client grant does not authorize this account and space",
        )?;
        if let ResourceScope::Exact(resource) = &leaf.scope {
            let record = state.resource(&resource.space, &resource.resource).ok_or(
                AuthorizationError::InvalidProof("missing client resource record"),
            )?;
            budget.verify(record)?;
            require(
                record.payload.host == leaf.audience.service,
                "client resource audience mismatch",
            )?;
        }
        let target = state.find_device(&leaf.audience.device)?;
        require(
            target.payload.subject == Actor::Service(leaf.audience.service.clone()),
            "client target service mismatch",
        )?;
        state.validate_certificate(
            target,
            now_ms,
            &mut budget,
            &mut dependencies,
            &mut BTreeSet::new(),
        )?;
        state.expand_authorities(now_ms, &mut budget, &mut dependencies)?;
        let mut resources = Vec::new();
        if let Some(document) = state.space(&space) {
            for hash in &document.resources {
                let Some(record) = state.resources.get(hash) else {
                    continue;
                };
                if record.payload.kind == ResourceKind::Project
                    && record.payload.host == leaf.audience.service
                    && scope_covers(
                        &leaf.scope,
                        &ResourceRef {
                            space: record.payload.space.clone(),
                            resource: record.payload.resource.clone(),
                        },
                    )
                {
                    budget.verify(record)?;
                    resources.push(record.clone());
                }
            }
        }
        let account_enrollment_rights = state
            .account(&account)
            .and_then(|a| {
                a.enrollment
                    .iter()
                    .find(|e| e.device == certificate.payload.device && now_ms < e.expires_at_ms)
            })
            .map(|e| {
                e.rights
                    .iter()
                    .filter(|r| {
                        **r != AccountRight::EnrollDevice
                            || certificate.payload.uses.contains(&DeviceUse::EnrollDevice)
                    })
                    .cloned()
                    .collect()
            })
            .unwrap_or_default();
        Ok(VerifiedClientAuthorization {
            scope: ClientScope {
                account_id: account,
                space_id: space,
                device_id: DeviceId::from_key(&key),
            },
            heads: dependencies.into_values().collect(),
            grant_leaf: bundle.grant_leaf,
            actions: leaf.actions.clone(),
            grant_scope: leaf.scope.clone(),
            audience: leaf.audience.clone(),
            audience_public_key: target.payload.public_key.clone(),
            constraints: leaf.constraints.clone(),
            account_enrollment_rights,
            resources,
        })
    }
    verify(
        bundle_json,
        pinned_roots_json,
        expected_account,
        expected_space,
        expected_device_public_key,
        now_ms,
    )
    .map_err(|e| e.to_string())
}

impl VerifiedPolicySet {
    fn expand_authorities(
        &self,
        now_ms: u64,
        budget: &mut Budget,
        dependencies: &mut BTreeMap<PolicyDomain, PolicyStamp>,
    ) -> Result<()> {
        let mut checked = BTreeSet::new();
        loop {
            for (domain, stamp) in &budget.dependencies {
                require(
                    self.head(domain) == Some(stamp),
                    "authorization dependency changed",
                )?;
                if let Some(previous) = dependencies.insert(domain.clone(), stamp.clone()) {
                    require(previous == *stamp, "conflicting authorization dependency")?;
                }
            }
            if dependencies.len() > MAX_POLICY_DOMAINS {
                return Err(AuthorizationError::BudgetExceeded);
            }
            let pending: Vec<_> = dependencies
                .keys()
                .filter(|d| !checked.contains(*d))
                .cloned()
                .collect();
            if pending.is_empty() {
                break;
            }
            for domain in pending {
                checked.insert(domain.clone());
                let authority = match &domain {
                    PolicyDomain::Account(id) => self.account(id).map(|d| &d.authority),
                    PolicyDomain::Space(id) => self.space(id).map(|d| &d.authority),
                }
                .ok_or(AuthorizationError::PolicyConflict)?;
                if self
                    .devices
                    .values()
                    .any(|c| c.payload.device == authority.device)
                {
                    let certificate = self.find_device(&authority.device)?;
                    require(
                        certificate.payload.subject == Actor::Service(authority.service.clone()),
                        "authority certificate is not its service",
                    )?;
                    self.validate_certificate(
                        certificate,
                        now_ms,
                        budget,
                        dependencies,
                        &mut BTreeSet::new(),
                    )?;
                }
            }
        }
        Ok(())
    }
    fn verify_policy_receipts(
        &self,
        commit: &Signed<PolicyCommit>,
        receipts: &[Signed<FreshnessReceipt>],
        domain: &PolicyDomain,
        now_ms: u64,
        budget: &mut Budget,
    ) -> Result<()> {
        let mut dependencies = budget.dependencies.clone();
        self.dependency(domain, &mut dependencies)?;
        self.expand_authorities(now_ms, budget, &mut dependencies)?;
        let expected: Vec<_> = dependencies.into_values().collect();
        require(
            commit.payload.dependencies == expected,
            "policy commit dependency set mismatch",
        )?;
        if receipts.len() != expected.len() {
            return Err(AuthorizationError::NeedFreshness(
                expected.into_iter().map(|s| s.domain).collect(),
            ));
        }
        let hash = commit.challenge_hash()?;
        let mut seen = BTreeSet::new();
        for signed in receipts {
            budget.verify(signed)?;
            let r = &signed.payload;
            require(
                r.version == AUTH_VERSION
                    && r.challenge_hash == hash
                    && expected.contains(&r.current)
                    && seen.insert(r.current.domain.clone())
                    && r.issued_at_ms >= commit.payload.issued_at_ms
                    && r.issued_at_ms <= commit.payload.accepted_at_ms
                    && r.expires_at_ms <= commit.payload.expires_at_ms,
                "policy freshness receipt mismatch",
            )?;
            validate_window(r.issued_at_ms, r.expires_at_ms, now_ms)?;
            let authority = match &r.current.domain {
                PolicyDomain::Account(id) => self.account(id).map(|d| &d.authority),
                PolicyDomain::Space(id) => self.space(id).map(|d| &d.authority),
            }
            .ok_or(AuthorizationError::PolicyConflict)?;
            require(
                signed.issuer == authority.public_key,
                "policy receipt is not from current authority",
            )?;
        }
        Ok(())
    }
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen(js_name = signInvocation))]
pub fn sign_invocation(invocation_json: &str, identity: &Identity) -> crate::Result<String> {
    fn sign(json: &str, identity: &Identity) -> Result<String> {
        require(json.len() <= MAX_PROOF_BYTES, "invocation too large")?;
        let invocation: Invocation = serde_json::from_str(json)
            .map_err(|_| AuthorizationError::InvalidProof("invalid invocation JSON"))?;
        require(
            invocation.version == AUTH_VERSION
                && invocation.device == DeviceId::from_key(&PublicKey::of(identity)),
            "invocation signing device mismatch",
        )?;
        serde_json::to_string(&Signed::sign(invocation, identity)?)
            .map_err(|_| AuthorizationError::InvalidProof("invocation encoding failed"))
    }
    sign(invocation_json, identity).map_err(|e| e.to_string())
}
/// Raw transport fingerprint for a pinned Ed25519 public key.
#[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen(js_name = nodeFingerprint))]
pub fn node_fingerprint(public_key_hex: &str) -> crate::Result<String> {
    let key = PublicKey::parse(public_key_hex).map_err(|e| e.to_string())?;
    Ok(hex::encode(Sha256::digest(crate::decode32(key.as_str())?)))
}
#[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen(js_name = deriveAuthorizationId))]
pub fn derive_authorization_id(kind: &str, input: &str) -> crate::Result<String> {
    fn derive(kind: &str, input: &str) -> Result<String> {
        Ok(match kind {
            "account" => AccountId::from_root(&PublicKey::parse(input)?).to_string(),
            "device" => DeviceId::from_key(&PublicKey::parse(input)?).to_string(),
            "service" => ServiceId::from_key(&PublicKey::parse(input)?).to_string(),
            "personal_space" => SpaceId::personal(&AccountId::parse(input)?).to_string(),
            "team_space" => SpaceId::team(&PublicKey::parse(input)?).to_string(),
            _ => return Err(AuthorizationError::InvalidProof("unknown identifier kind")),
        })
    }
    derive(kind, input).map_err(|e| e.to_string())
}
#[derive(Serialize)]
struct PreparedSigning {
    domain: &'static str,
    signing_bytes: String,
    object_hash: ObjectHash,
}
fn prepare_signed<T: SignedObject + for<'de> Deserialize<'de>>(
    json: &str,
    issuer: &PublicKey,
) -> Result<PreparedSigning> {
    let payload: T = serde_json::from_str(json)
        .map_err(|_| AuthorizationError::InvalidProof("invalid typed signing payload"))?;
    let bytes = canonical(&Unsigned {
        issuer,
        payload: &payload,
    })?;
    let object_hash = ObjectHash(format!("h2_{}", digest(T::DOMAIN, &bytes)));
    Ok(PreparedSigning {
        domain: T::DOMAIN,
        signing_bytes: String::from_utf8(bytes)
            .map_err(|_| AuthorizationError::InvalidProof("invalid signing UTF-8"))?,
        object_hash,
    })
}
/// Returns the exact bytes for Identity.sign. It does not authorize the payload.
#[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen(js_name = prepareAuthorizationSigning))]
pub fn prepare_authorization_signing(
    kind: &str,
    payload_json: &str,
    issuer_public_key: &str,
) -> crate::Result<String> {
    fn prepare(kind: &str, json: &str, issuer: &str) -> Result<String> {
        require(json.len() <= 262_144, "signing payload too large")?;
        let issuer = PublicKey::parse(issuer)?;
        let prepared = match kind {
            "account_document" => prepare_signed::<AccountDocument>(json, &issuer)?,
            "space_document" => prepare_signed::<SpaceDocument>(json, &issuer)?,
            "device_certificate" => prepare_signed::<DeviceCertificate>(json, &issuer)?,
            "membership" => prepare_signed::<Membership>(json, &issuer)?,
            "resource" => prepare_signed::<ResourceRecord>(json, &issuer)?,
            "authenticator" => prepare_signed::<AuthenticatorRecord>(json, &issuer)?,
            "grant" => prepare_signed::<Grant>(json, &issuer)?,
            "invocation" => prepare_signed::<Invocation>(json, &issuer)?,
            "policy_commit" => prepare_signed::<PolicyCommit>(json, &issuer)?,
            "freshness" => prepare_signed::<FreshnessReceipt>(json, &issuer)?,
            "account_root_acceptance" | "account_recovery" => {
                let document: AccountDocument = serde_json::from_str(json).map_err(|_| {
                    AuthorizationError::InvalidProof("invalid account document JSON")
                })?;
                let domain = if kind == "account_root_acceptance" {
                    "account-root-accept-v2"
                } else {
                    "account-recovery-v2"
                };
                let bytes = document.governance_bytes()?;
                PreparedSigning {
                    domain,
                    object_hash: ObjectHash(format!("h2_{}", digest(domain, &bytes))),
                    signing_bytes: String::from_utf8(bytes)
                        .map_err(|_| AuthorizationError::InvalidProof("invalid UTF-8"))?,
                }
            }
            "space_root_acceptance" | "space_recovery" => {
                let document: SpaceDocument = serde_json::from_str(json)
                    .map_err(|_| AuthorizationError::InvalidProof("invalid space document JSON"))?;
                let domain = if kind == "space_root_acceptance" {
                    "space-root-accept-v2"
                } else {
                    "space-recovery-v2"
                };
                let bytes = document.governance_bytes()?;
                PreparedSigning {
                    domain,
                    object_hash: ObjectHash(format!("h2_{}", digest(domain, &bytes))),
                    signing_bytes: String::from_utf8(bytes)
                        .map_err(|_| AuthorizationError::InvalidProof("invalid UTF-8"))?,
                }
            }
            "policy_commit_challenge" => {
                let mut payload: PolicyCommit = serde_json::from_str(json)
                    .map_err(|_| AuthorizationError::InvalidProof("invalid policy commit JSON"))?;
                payload.accepted_at_ms = 0;
                let bytes = canonical(&Unsigned {
                    issuer: &issuer,
                    payload: &payload,
                })?;
                let domain = "policy-commit-challenge-v2";
                PreparedSigning {
                    domain,
                    object_hash: ObjectHash(format!("h2_{}", digest(domain, &bytes))),
                    signing_bytes: String::from_utf8(bytes)
                        .map_err(|_| AuthorizationError::InvalidProof("invalid UTF-8"))?,
                }
            }
            "device_possession" => {
                let certificate: DeviceCertificate = serde_json::from_str(json)
                    .map_err(|_| AuthorizationError::InvalidProof("invalid certificate JSON"))?;
                require(
                    certificate.public_key == issuer,
                    "possession signing key mismatch",
                )?;
                let bytes = certificate.possession_bytes()?;
                PreparedSigning {
                    domain: "device-possession-v2",
                    object_hash: ObjectHash(format!(
                        "h2_{}",
                        digest("device-possession-v2", &bytes)
                    )),
                    signing_bytes: String::from_utf8(bytes)
                        .map_err(|_| AuthorizationError::InvalidProof("invalid UTF-8"))?,
                }
            }
            _ => {
                return Err(AuthorizationError::InvalidProof(
                    "unknown signing object kind",
                ));
            }
        };
        serde_json::to_string(&prepared)
            .map_err(|_| AuthorizationError::InvalidProof("prepared signing encoding failed"))
    }
    prepare(kind, payload_json, issuer_public_key).map_err(|e| e.to_string())
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen(js_name = deriveAuthorizationBodyHash))]
pub fn derive_authorization_body_hash(body_json: &str) -> crate::Result<String> {
    fn hash(json: &str) -> Result<String> {
        require(json.len() <= MAX_PROOF_BYTES, "request body too large")?;
        let value: serde_json::Value = serde_json::from_str(json)
            .map_err(|_| AuthorizationError::InvalidProof("invalid body JSON"))?;
        Ok(ObjectHash::body(&value)?.to_string())
    }
    hash(body_json).map_err(|e| e.to_string())
}
#[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen(js_name = signClientInvocation))]
pub fn sign_client_invocation(
    auth: &VerifiedClientAuthorization,
    template_json: &str,
    request_json: &str,
    expected_action: &str,
    operation_id: &str,
    identity: &Identity,
    now_ms: f64,
) -> crate::Result<String> {
    sign_client_invocation_inner(
        auth,
        template_json,
        request_json,
        expected_action,
        operation_id,
        identity,
        now_ms,
        &DeviceId::from_key(&PublicKey::of(identity)),
    )
    .map_err(|e| e.to_string())
}

/// The Gateway key comes from the saved, independently pinned Noise connection.
#[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen(js_name = signForwardedClientInvocation))]
pub fn sign_forwarded_client_invocation(
    auth: &VerifiedClientAuthorization,
    template_json: &str,
    request_json: &str,
    expected_action: &str,
    operation_id: &str,
    expected_gateway_public_key: &str,
    identity: &Identity,
    now_ms: f64,
) -> crate::Result<String> {
    let gateway = PublicKey::parse(expected_gateway_public_key).map_err(|e| e.to_string())?;
    require(
        gateway != PublicKey::of(identity),
        "gateway cannot be client key",
    )
    .map_err(|e| e.to_string())?;
    sign_client_invocation_inner(
        auth,
        template_json,
        request_json,
        expected_action,
        operation_id,
        identity,
        now_ms,
        &DeviceId::from_key(&gateway),
    )
    .map_err(|e| e.to_string())
}

fn sign_client_invocation_inner(
    auth: &VerifiedClientAuthorization,
    json: &str,
    body: &str,
    action: &str,
    operation: &str,
    key: &Identity,
    now: f64,
    presenter: &DeviceId,
) -> Result<String> {
    require(
        json.len() <= MAX_PROOF_BYTES
            && body.len() <= MAX_PROOF_BYTES
            && now.is_finite()
            && now >= 0.0
            && now.fract() == 0.0
            && now <= 9_007_199_254_740_991.0,
        "invalid client signing input",
    )?;
    let invocation: Invocation = serde_json::from_str(json)
        .map_err(|_| AuthorizationError::InvalidProof("invalid invocation JSON"))?;
    let action: Action = serde_json::from_value(serde_json::Value::String(action.to_owned()))
        .map_err(|_| AuthorizationError::InvalidProof("unknown expected action"))?;
    let request: serde_json::Value = serde_json::from_str(body)
        .map_err(|_| AuthorizationError::InvalidProof("invalid request JSON"))?;
    let actor = Actor::Account(auth.scope.account_id.clone());
    require(
        invocation.version == AUTH_VERSION
            && invocation.actor == actor
            && invocation.initiator == actor
            && invocation.initiating_grant.is_none()
            && invocation.device == auth.scope.device_id
            && invocation.device == DeviceId::from_key(&PublicKey::of(key))
            && invocation.presenter == *presenter
            && invocation.grant_leaf == auth.grant_leaf
            && invocation.expected_policies == auth.heads
            && invocation.audience == auth.audience
            && invocation.resource.space == auth.scope.space_id
            && scope_covers(&auth.grant_scope, &invocation.resource)
            && invocation.action == action
            && auth.actions.contains(&action)
            && invocation.operation_id == operation
            && valid_operation_id(operation)
            && invocation.body_hash == ObjectHash::body(&request)?
            && context_tighter(&auth.constraints.context, &invocation.context),
        "invocation template does not match client request",
    )?;
    if auth.constraints.require_webauthn_uv {
        return Err(AuthorizationError::UserPresenceRequired);
    }
    validate_window(
        invocation.not_before_ms,
        invocation.expires_at_ms,
        now as u64,
    )?;
    require(
        invocation.expires_at_ms - invocation.not_before_ms <= INVOCATION_MAX_AGE_MS,
        "invocation lifetime too long",
    )?;
    serde_json::to_string(&Signed::sign(invocation, key)?)
        .map_err(|_| AuthorizationError::InvalidProof("client invocation encoding failed"))
}

impl VerifiedClientAuthorization {
    pub fn scope(&self) -> &ClientScope {
        &self.scope
    }
    pub fn heads(&self) -> &[PolicyStamp] {
        &self.heads
    }
    pub fn grant_leaf(&self) -> &ObjectHash {
        &self.grant_leaf
    }
    pub fn audience(&self) -> &Audience {
        &self.audience
    }
    pub fn audience_public_key(&self) -> &PublicKey {
        &self.audience_public_key
    }
    pub fn actions(&self) -> &[Action] {
        &self.actions
    }
    pub fn account_enrollment_rights(&self) -> &[AccountRight] {
        &self.account_enrollment_rights
    }
    pub fn resources(&self) -> &[Signed<ResourceRecord>] {
        &self.resources
    }
}
/// Reconstruct signed history from independent local pins. It grants no fresh write lease.
pub fn replay_policies(pins: &[PolicyPin], journal: &[JournalEntry]) -> Result<VerifiedPolicySet> {
    require(
        pins.len() <= MAX_POLICY_DOMAINS
            && pins.windows(2).all(|p| p[0].domain < p[1].domain)
            && journal.len() <= MAX_POLICY_HISTORY * MAX_POLICY_DOMAINS,
        "invalid policy history bounds",
    )?;
    let mut state = VerifiedPolicySet::new();
    for entry in journal.iter().cloned() {
        match entry {
            JournalEntry::AccountGenesis(signed) => {
                let domain = PolicyDomain::Account(signed.payload.account.clone());
                let pin = pins.iter().find(|p| p.domain == domain).ok_or(
                    AuthorizationError::InvalidProof("account genesis is not pinned"),
                )?;
                state.trust_account_genesis(&pin.genesis_root, signed)?;
            }
            JournalEntry::SpaceGenesis(signed) => {
                let domain = PolicyDomain::Space(signed.payload.space.clone());
                let pin = pins.iter().find(|p| p.domain == domain).ok_or(
                    AuthorizationError::InvalidProof("space genesis is not pinned"),
                )?;
                state.trust_space_genesis(&pin.genesis_root, signed)?;
            }
            JournalEntry::Account(signed) => {
                state.install_account(signed)?;
            }
            JournalEntry::Space(signed) => {
                state.install_space(signed)?;
            }
            JournalEntry::Device(signed) => {
                let at = signed.payload.not_before_ms;
                state.install_device(signed, at)?;
            }
            JournalEntry::Resource(signed) => {
                state.install_resource(signed)?;
            }
            JournalEntry::Authenticator(signed) => {
                let at = signed.payload.not_before_ms;
                state.install_authenticator(signed, at)?;
            }
            JournalEntry::Membership(signed) => {
                let at = signed.payload.not_before_ms;
                state.install_membership(signed, at)?;
            }
            JournalEntry::Grant(signed) => {
                let at = signed.payload.not_before_ms;
                state.install_grant(signed, at)?;
            }
            JournalEntry::AccountCommit(change) => {
                state.replay_account_committed(change)?;
            }
            JournalEntry::SpaceCommit(change) => {
                state.replay_space_committed(change)?;
            }
        }
    }
    for pin in pins {
        if let Some(minimum) = &pin.minimum_head {
            require(
                minimum.domain == pin.domain && state.roots.contains_key(minimum),
                "client policy rollback",
            )?;
        }
    }
    Ok(state)
}

pub mod task_binding;
pub use task_binding::verify_task_binding;

pub mod events;
