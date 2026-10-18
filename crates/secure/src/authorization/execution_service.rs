//! Execution services require their physical owner's current personal root.

use super::{
    bootstrap::recovery::RecoveryMaterial,
    client_policy_import::require_confirmed_client_policy,
    execution_handoff::ExecutionProofUsage,
    governance,
    policy_import::{VerifiedPolicySnapshot, merge_same_head_snapshots},
    *,
};
use crate::signature_verification::{
    ScopedSignatureVerification, SignatureVerification, SignatureVerificationCounts,
};

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ExecutionServiceRegistrationIntent {
    pub version: u16,
    pub account: AccountId,
    pub source_project: ResourceRef,
    pub source_grant: ObjectHash,
    pub execution_resource: ResourceRef,
    pub target_node_public_key: PublicKey,
    pub expected_heads: Vec<PolicyStamp>,
    pub policy_request_hash: ObjectHash,
    pub nonce: Nonce,
    pub issued_at_ms: u64,
    pub expires_at_ms: u64,
}
impl sealed::Sealed for ExecutionServiceRegistrationIntent {}
impl SignedObject for ExecutionServiceRegistrationIntent {
    const DOMAIN: &'static str = "execution-service-registration-intent-v2";
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ExecutionServiceRootApproval {
    pub version: u16,
    pub intent_hash: ObjectHash,
}
impl sealed::Sealed for ExecutionServiceRootApproval {}
impl SignedObject for ExecutionServiceRootApproval {
    const DOMAIN: &'static str = "execution-service-registration-root-approval-v2";
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ExecutionServiceRegistrationRequest {
    pub intent: Signed<ExecutionServiceRegistrationIntent>,
    pub root_approval: Signed<ExecutionServiceRootApproval>,
    pub request: governance::SpaceGovernanceRequest,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ExecutionServiceInstallIntent {
    pub version: u16,
    pub account: AccountId,
    pub registration_hash: ObjectHash,
    pub committed_hash: ObjectHash,
    pub target_node_public_key: PublicKey,
    pub canonical_root: String,
    pub expected_heads: Vec<PolicyStamp>,
    pub challenge_hash: ObjectHash,
    pub nonce: Nonce,
    pub issued_at_ms: u64,
    pub expires_at_ms: u64,
}
impl sealed::Sealed for ExecutionServiceInstallIntent {}
impl SignedObject for ExecutionServiceInstallIntent {
    const DOMAIN: &'static str = "execution-service-install-intent-v2";
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ExecutionServiceInstallRequest {
    pub registration: ExecutionServiceRegistrationRequest,
    pub committed: Committed<SpaceDocument>,
    pub challenge: Signed<FreshnessChallenge>,
    pub intent: Signed<ExecutionServiceInstallIntent>,
}

pub struct VerifiedExecutionServiceInstall {
    source_project: ResourceRef,
    execution_resource: ResourceRef,
    management_parent: ObjectHash,
    canonical_root: String,
    nonce: Nonce,
    request_hash: ObjectHash,
    expires_at_ms: u64,
    proof_counts: RegistrationProofCounts,
}
impl VerifiedExecutionServiceInstall {
    pub fn source_project(&self) -> &ResourceRef {
        &self.source_project
    }
    pub fn execution_resource(&self) -> &ResourceRef {
        &self.execution_resource
    }
    pub fn management_parent(&self) -> &ObjectHash {
        &self.management_parent
    }
    pub fn canonical_root(&self) -> &str {
        &self.canonical_root
    }
    pub fn nonce(&self) -> &Nonce {
        &self.nonce
    }
    pub fn request_hash(&self) -> &ObjectHash {
        &self.request_hash
    }
    pub fn expires_at_ms(&self) -> u64 {
        self.expires_at_ms
    }
    pub fn proof_usage_json(&self) -> crate::Result<String> {
        json(&self.proof_counts)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ExecutionServiceSelection {
    pub source_project: ResourceRef,
    pub execution_resource: ResourceRef,
    pub management_parent: ObjectHash,
}

pub struct VerifiedExecutionServiceRegistrationRequest {
    expected_heads: Vec<PolicyStamp>,
    commit_heads: Vec<PolicyStamp>,
    expires: u64,
    budget: Budget,
    max_depth: usize,
    crypto_counts: Option<SignatureVerificationCounts>,
}
impl VerifiedExecutionServiceRegistrationRequest {
    pub fn expected_heads(&self) -> &[PolicyStamp] {
        &self.expected_heads
    }
    pub fn commit_heads(&self) -> &[PolicyStamp] {
        &self.commit_heads
    }
    pub fn expires_at_ms(&self) -> u64 {
        self.expires
    }
    pub fn proof_usage_json(&self) -> crate::Result<String> {
        json(&self.proof_counts())
    }
    pub fn proof_usage(&self) -> Result<ExecutionProofUsage> {
        let mut usage = ExecutionProofUsage::default();
        usage.record_budget(&self.budget)?;
        usage.record_chain_depth(self.max_depth);
        if let Some(counts) = &self.crypto_counts {
            usage.record_actual_signature_counts(counts.clone());
        }
        Ok(usage)
    }
    pub fn proof_counts(&self) -> RegistrationProofCounts {
        RegistrationProofCounts {
            objects: self.budget.objects.len(),
            bytes: self.budget.bytes,
            signatures: self.budget.signatures.len() + self.budget.external_signatures.len(),
            max_depth: self.max_depth,
            actual_verifications: self
                .crypto_counts
                .as_ref()
                .map_or(0, |counts| counts.actual_verifications),
            cache_hits: self
                .crypto_counts
                .as_ref()
                .map_or(0, |counts| counts.cache_hits),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RegistrationProofCounts {
    pub objects: usize,
    pub bytes: usize,
    pub signatures: usize,
    pub max_depth: usize,
    pub actual_verifications: usize,
    pub cache_hits: usize,
}

fn parse<T: for<'de> Deserialize<'de>>(value: &str) -> Result<T> {
    require(
        value.len() <= MAX_PROOF_BYTES,
        "execution registration JSON too large",
    )?;
    serde_json::from_str(value)
        .map_err(|_| AuthorizationError::InvalidProof("invalid execution registration JSON"))
}
fn json<T: Serialize>(value: &T) -> crate::Result<String> {
    serde_json::to_string(value).map_err(|_| "execution registration encoding failed".to_owned())
}
fn management_actions() -> Vec<Action> {
    vec![
        Action::ExecutionOpen,
        Action::ExecutionSend,
        Action::ExecutionCommand,
        Action::ExecutionSteer,
        Action::ExecutionInterrupt,
        Action::ExecutionRespondApproval,
        Action::ExecutionSubmit,
        Action::ExecutionResolve,
        Action::OperationStatusRead,
        Action::OperationResolve,
        Action::ResourceManage,
        Action::GrantIssue,
        Action::GrantRevoke,
    ]
}

#[allow(clippy::too_many_arguments)]
fn authorization(
    state: &VerifiedPolicySet,
    account: &AccountId,
    signer: &PublicKey,
    source: &ResourceRef,
    source_grant: &ObjectHash,
    node: &PublicKey,
    now: u64,
    budget: &mut Budget,
    require_personal_owner: bool,
) -> Result<VerifiedExecutionServiceRegistrationRequest> {
    let personal = SpaceId::personal(account);
    let client = state.find_device(&DeviceId::from_key(signer))?;
    require(
        client.payload.owner == PolicyDomain::Account(account.clone())
            && client.payload.subject == Actor::Account(account.clone())
            && client.payload.uses.contains(&DeviceUse::Noise)
            && client.payload.uses.contains(&DeviceUse::AccountInvocation),
        "execution registration requires a current account client",
    )?;
    let mut dependencies = BTreeMap::new();
    require(
        state.actor_for_signer(signer, now, budget, &mut dependencies)?
            == Actor::Account(account.clone()),
        "execution registration requires the current physical device owner's account client",
    )?;
    state.governance_capability(
        source_grant,
        signer,
        &source.space,
        Some(Action::ResourceManage),
        Some(&source.resource),
        now,
        budget,
    )?;
    let target = state.find_device(&DeviceId::from_key(node))?;
    for space in std::iter::once(&source.space).chain(require_personal_owner.then_some(&personal)) {
        state.ensure_personal_current(
            state
                .space(space)
                .ok_or(AuthorizationError::PolicyConflict)?,
        )?;
        state.validate_service_reference(space, target)?;
    }
    require(
        target.payload.public_key == *node
            && target.payload.owner == PolicyDomain::Account(account.clone())
            && target.payload.subject == Actor::Service(ServiceId::from_key(node))
            && target.payload.uses.contains(&DeviceUse::Noise),
        "execution registration cannot configure another account's physical node",
    )?;
    let record =
        state
            .resource(&source.space, &source.resource)
            .ok_or(AuthorizationError::InvalidProof(
                "missing execution source project",
            ))?;
    let source_audience = &state
        .grant(source_grant)
        .ok_or(AuthorizationError::Revoked)?
        .payload
        .audience;
    let source_host = state.find_device(&source_audience.device)?;
    require(
        record.payload.kind == ResourceKind::Project
            && record.payload.host == source_audience.service
            && source_host.payload.subject == Actor::Service(source_audience.service.clone()),
        "execution management grant belongs to another Project host",
    )?;
    budget.verify(record)?;
    state.validate_service_reference(&source.space, source_host)?;
    state.validate_certificate(
        source_host,
        now,
        budget,
        &mut dependencies,
        &mut BTreeSet::new(),
    )?;
    state.validate_certificate(target, now, budget, &mut dependencies, &mut BTreeSet::new())?;
    dependencies.extend(budget.dependencies.clone());
    state.dependency(
        &PolicyDomain::Space(source.space.clone()),
        &mut dependencies,
    )?;
    if require_personal_owner {
        state.dependency(&PolicyDomain::Space(personal.clone()), &mut dependencies)?;
    }
    state.expand_authorities(now, budget, &mut dependencies)?;
    let all_dependencies = budget.dependencies.clone();
    budget.dependencies.clear();
    let mut commit_heads = BTreeMap::new();
    if require_personal_owner {
        state.dependency(&PolicyDomain::Space(personal), &mut commit_heads)?;
        state.expand_authorities(now, budget, &mut commit_heads)?;
    }
    budget.dependencies = all_dependencies;
    Ok(VerifiedExecutionServiceRegistrationRequest {
        expected_heads: dependencies.into_values().collect(),
        commit_heads: commit_heads.into_values().collect(),
        expires: budget.expires_at_ms,
        budget: budget.clone(),
        max_depth: state.grant_chain(Some(source_grant))?.len(),
        crypto_counts: SignatureVerification::current_counts(),
    })
}

pub fn verify_execution_service_source_management(
    state: &VerifiedPolicySet,
    account: &AccountId,
    signer: &PublicKey,
    source: &ResourceRef,
    source_grant: &ObjectHash,
    node: &PublicKey,
    now: u64,
) -> Result<VerifiedExecutionServiceRegistrationRequest> {
    let _signature_scope = ScopedSignatureVerification::new();
    authorization(
        state,
        account,
        signer,
        source,
        source_grant,
        node,
        now,
        &mut Budget::default(),
        false,
    )
}

#[allow(clippy::too_many_arguments)]
fn verify_management_parent(
    state: &VerifiedPolicySet,
    account: &AccountId,
    signer: &PublicKey,
    resource: &ResourceRef,
    parent: &ObjectHash,
    node: &PublicKey,
    now: u64,
    budget: &mut Budget,
) -> Result<()> {
    let personal = SpaceId::personal(account);
    require(
        resource.space == personal,
        "execution service must belong to the physical owner's personal space",
    )?;
    let mut dependencies = BTreeMap::new();
    let client = state.find_device(&DeviceId::from_key(signer))?;
    require(
        client.payload.owner == PolicyDomain::Account(account.clone())
            && client.payload.uses.contains(&DeviceUse::Noise)
            && client.payload.uses.contains(&DeviceUse::AccountInvocation)
            && state.actor_for_signer(signer, now, budget, &mut dependencies)?
                == Actor::Account(account.clone()),
        "execution selection requires the current physical owner's account client",
    )?;
    let target = state.find_device(&DeviceId::from_key(node))?;
    require(
        target.payload.public_key == *node
            && target.payload.owner == PolicyDomain::Account(account.clone())
            && target.payload.subject == Actor::Service(ServiceId::from_key(node))
            && target.payload.uses.contains(&DeviceUse::Noise),
        "execution selection cannot configure another account's physical node",
    )?;
    let document = state
        .space(&personal)
        .ok_or(AuthorizationError::PolicyConflict)?;
    state.ensure_personal_current(document)?;
    state.validate_service_reference(&personal, target)?;
    state.validate_certificate(target, now, budget, &mut dependencies, &mut BTreeSet::new())?;
    let record = state.resource(&resource.space, &resource.resource).ok_or(
        AuthorizationError::InvalidProof("missing execution service resource"),
    )?;
    require(
        record.payload.kind == ResourceKind::ExecutionService
            && record.payload.host == ServiceId::from_key(node),
        "selected resource is not an execution service on the physical node",
    )?;
    budget.verify(record)?;
    let grant = state.grant(parent).ok_or(AuthorizationError::InvalidProof(
        "missing execution service management parent",
    ))?;
    require(
        grant.payload.owner_space == personal
            && grant.payload.scope == ResourceScope::Exact(resource.clone())
            && grant.payload.grantee == Grantee::Actor(Actor::Account(account.clone()))
            && grant.payload.actions == management_actions()
            && grant.payload.audience
                == (Audience {
                    service: ServiceId::from_key(node),
                    device: DeviceId::from_key(node),
                })
            && grant.payload.parent.is_none()
            && grant.payload.remaining_delegations == 1
            && grant.payload.constraints == GrantConstraints::default(),
        "selected grant is not the execution service management parent",
    )?;
    let path = state.grant_chain(Some(parent))?;
    state.verify_grant_path(&path, now, budget, &mut dependencies, true)?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
pub fn project_execution_service_selection(
    state: &VerifiedPolicySet,
    account: &AccountId,
    signer: &PublicKey,
    source: &ResourceRef,
    resource: &ResourceRef,
    parent: &ObjectHash,
    node: &PublicKey,
    now: u64,
    usage: &mut ExecutionProofUsage,
) -> Result<ExecutionServiceSelection> {
    let _signature_scope = ScopedSignatureVerification::new();
    let mut budget = Budget::default();
    let checked = verify_management_parent(
        state,
        account,
        signer,
        resource,
        parent,
        node,
        now,
        &mut budget,
    );
    usage.record_budget(&budget)?;
    if let Ok(path) = state.grant_chain(Some(parent)) {
        usage.record_chain_depth(path.len());
    }
    usage.enforce_limits()?;
    checked?;
    Ok(ExecutionServiceSelection {
        source_project: source.clone(),
        execution_resource: resource.clone(),
        management_parent: parent.clone(),
    })
}

/// The authenticated physical Node supplies the source mapping. Pins remain independent.
#[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen(js_name = verifyExecutionServiceSelection))]
pub fn verify_execution_service_selection(
    authorization: &VerifiedClientAuthorization,
    selection_json: &str,
    expected_source_project_json: &str,
    expected_execution_resource_json: &str,
    expected_target_node_public_key: &str,
    now_ms: f64,
) -> crate::Result<VerifiedClientAuthorization> {
    let _signature_scope = ScopedSignatureVerification::new();
    let verify = || -> Result<_> {
        let selection: ExecutionServiceSelection = parse(selection_json)?;
        let source: ResourceRef = parse(expected_source_project_json)?;
        let resource: ResourceRef = parse(expected_execution_resource_json)?;
        require(
            selection.source_project == source && selection.execution_resource == resource,
            "execution selection differs from the requested source and service",
        )?;
        let snapshot = require_confirmed_client_policy(authorization)?;
        let node = PublicKey::parse(expected_target_node_public_key)?;
        let now = bootstrap::timestamp(now_ms)?;
        let signer = snapshot
            .state()
            .find_device(&authorization.scope.device_id)?
            .payload
            .public_key
            .clone();
        require(
            authorization.grant_leaf == selection.management_parent
                && authorization.scope.space_id == resource.space
                && authorization.audience_public_key == node,
            "execution selection differs from its imported authorization",
        )?;
        let mut parent_budget = Budget::default();
        verify_management_parent(
            snapshot.state(),
            &authorization.scope.account_id,
            &signer,
            &resource,
            &selection.management_parent,
            &node,
            now,
            &mut parent_budget,
        )?;
        let mut current = revalidate_selected(authorization, snapshot, &resource, &signer, now)?;
        let mut usage = ExecutionProofUsage::default();
        usage.record_budget(&parent_budget)?;
        usage.record_budget(&current.budget)?;
        usage.record_metadata(&selection)?;
        usage.record_chain_depth(
            snapshot
                .state()
                .grant_chain(Some(&selection.management_parent))?
                .len(),
        );
        finish_native_usage(&mut usage)?;
        current.client_policy_usage = Some(usage);
        Ok(current)
    };
    verify().map_err(|error| error.to_string())
}

pub fn verify_execution_service_registration(
    state: &VerifiedPolicySet,
    signed: &ExecutionServiceRegistrationRequest,
    local_node: &PublicKey,
    now: u64,
) -> Result<VerifiedExecutionServiceRegistrationRequest> {
    let _signature_scope = ScopedSignatureVerification::new();
    require(
        canonical(signed)?.len() <= MAX_PROOF_BYTES,
        "execution registration too large",
    )?;
    let mut budget = Budget::default();
    budget.verify(&signed.intent)?;
    budget.verify(&signed.root_approval)?;
    let intent = &signed.intent.payload;
    let request = &signed.request;
    validate_window(intent.issued_at_ms, intent.expires_at_ms, now)?;
    let personal = SpaceId::personal(&intent.account);
    let account = state
        .account(&intent.account)
        .ok_or(AuthorizationError::PolicyConflict)?;
    let previous = state
        .space(&personal)
        .ok_or(AuthorizationError::PolicyConflict)?;
    require(
        intent.version == AUTH_VERSION
            && intent.target_node_public_key == *local_node
            && intent.execution_resource.space == personal
            && intent.source_project != intent.execution_resource
            && request.proposal.payload.space == personal
            && request.proposal.issuer == account.current_root
            && signed.root_approval.issuer == account.current_root
            && signed.root_approval.payload.version == AUTH_VERSION
            && signed.root_approval.payload.intent_hash == ObjectHash::body(&signed.intent)?
            && intent.policy_request_hash == ObjectHash::body(request)?
            && intent.nonce == request.nonce,
        "execution registration differs from its current root-approved intent",
    )?;
    let [
        JournalEntry::Resource(resource),
        JournalEntry::Grant(parent),
    ] = request.new_objects.as_slice()
    else {
        return Err(AuthorizationError::InvalidProof(
            "execution registration requires one resource and parent grant",
        ));
    };
    let current = state
        .head(&PolicyDomain::Space(personal.clone()))
        .ok_or(AuthorizationError::PolicyConflict)?;
    require(
        resource.issuer == account.current_root
            && resource.payload.version == AUTH_VERSION
            && resource.payload.space == personal
            && resource.payload.resource == intent.execution_resource.resource
            && resource.payload.kind == ResourceKind::ExecutionService
            && resource.payload.host == ServiceId::from_key(local_node)
            && resource.payload.issued_under == *current
            && state
                .resource(&personal, &resource.payload.resource)
                .is_none(),
        "execution registration contains another resource",
    )?;
    require(
        parent.issuer == account.current_root
            && parent.payload.version == AUTH_VERSION
            && parent.payload.owner_space == personal
            && parent.payload.scope == ResourceScope::Exact(intent.execution_resource.clone())
            && parent.payload.grantee == Grantee::Actor(Actor::Account(intent.account.clone()))
            && parent.payload.actions == management_actions()
            && parent.payload.audience
                == (Audience {
                    service: ServiceId::from_key(local_node),
                    device: DeviceId::from_key(local_node),
                })
            && parent.payload.constraints == GrantConstraints::default()
            && parent.payload.parent.is_none()
            && parent.payload.remaining_delegations == 1
            && parent.payload.issued_under == *current
            && parent.payload.not_before_ms == intent.issued_at_ms
            && intent.expires_at_ms <= parent.payload.expires_at_ms,
        "execution registration parent exceeds its explicit resource approval",
    )?;
    let mut expected = previous.clone();
    expected.previous = Some(current.clone());
    expected.sequence = expected
        .sequence
        .checked_add(1)
        .ok_or(AuthorizationError::PolicyConflict)?;
    expected.governance_grant = None;
    expected.root_acceptance = None;
    expected.recovery_approvals.clear();
    expected.resources.push(resource.hash()?);
    expected.resources.sort();
    expected.grants.push(parent.hash()?);
    expected.grants.sort();
    require(
        request.proposal.payload == expected,
        "execution registration changed unrelated space policy",
    )?;
    budget.verify(&request.proposal)?;
    budget.verify(resource)?;
    budget.verify(parent)?;
    let mut verified = authorization(
        state,
        &intent.account,
        &signed.intent.issuer,
        &intent.source_project,
        &intent.source_grant,
        local_node,
        now,
        &mut budget,
        true,
    )?;
    require(
        intent.expected_heads == verified.expected_heads
            && parent.payload.expires_at_ms <= verified.expires
            && intent.expires_at_ms <= verified.expires,
        "execution registration heads or capability expiry changed",
    )?;
    let mut candidate = state.clone();
    candidate.install_resource(resource.clone())?;
    candidate.install_grant(parent.clone(), now)?;
    candidate.install_space(request.proposal.clone())?;
    verified.crypto_counts = SignatureVerification::current_counts();
    Ok(verified)
}

pub fn execution_service_registration_challenge(
    state: &VerifiedPolicySet,
    request: &ExecutionServiceRegistrationRequest,
    local_node: &PublicKey,
    now: u64,
) -> Result<FreshnessChallenge> {
    let _signature_scope = ScopedSignatureVerification::new();
    let verified = verify_execution_service_registration(state, request, local_node, now)?;
    let intent = &request.intent;
    Ok(FreshnessChallenge {
        version: AUTH_VERSION,
        stage: CheckStage::Admit,
        invocation_hash: intent.hash()?,
        operation_id: format!("execution-service-register:{}", intent.payload.nonce),
        body_hash: ObjectHash::body(request)?,
        audience: Audience {
            service: ServiceId::from_key(local_node),
            device: DeviceId::from_key(local_node),
        },
        presenter: DeviceId::from_key(&intent.issuer),
        policies: verified.expected_heads,
        nonce: Nonce::random()?,
        issued_at_ms: now,
        expires_at_ms: now
            .saturating_add(EXECUTION_LEASE_MS)
            .min(intent.payload.expires_at_ms)
            .min(verified.expires),
    })
}

pub fn verify_execution_service_registration_freshness(
    state: &VerifiedPolicySet,
    request: &ExecutionServiceRegistrationRequest,
    challenge: &FreshnessChallenge,
    receipts: &[Signed<FreshnessReceipt>],
    local_node: &PublicKey,
    now: u64,
) -> Result<VerifiedExecutionServiceRegistrationRequest> {
    let _signature_scope = ScopedSignatureVerification::new();
    let mut verified = verify_execution_service_registration(state, request, local_node, now)?;
    check_registration_freshness(
        state,
        request,
        challenge,
        receipts,
        local_node,
        now,
        &mut verified,
    )?;
    verified.crypto_counts = SignatureVerification::current_counts();
    Ok(verified)
}

fn check_registration_freshness(
    state: &VerifiedPolicySet,
    request: &ExecutionServiceRegistrationRequest,
    challenge: &FreshnessChallenge,
    receipts: &[Signed<FreshnessReceipt>],
    local_node: &PublicKey,
    now: u64,
    verified: &mut VerifiedExecutionServiceRegistrationRequest,
) -> Result<()> {
    let intent = &request.intent;
    require(
        challenge.version == AUTH_VERSION
            && challenge.stage == CheckStage::Admit
            && challenge.invocation_hash == intent.hash()?
            && challenge.body_hash == ObjectHash::body(request)?
            && challenge.operation_id
                == format!("execution-service-register:{}", intent.payload.nonce)
            && challenge.audience
                == (Audience {
                    service: ServiceId::from_key(local_node),
                    device: DeviceId::from_key(local_node),
                })
            && challenge.presenter == DeviceId::from_key(&intent.issuer)
            && challenge.policies == verified.expected_heads
            && challenge.issued_at_ms >= intent.payload.issued_at_ms
            && challenge.expires_at_ms <= intent.payload.expires_at_ms
            && challenge.expires_at_ms <= verified.expires
            && challenge
                .expires_at_ms
                .saturating_sub(challenge.issued_at_ms)
                <= EXECUTION_LEASE_MS,
        "execution registration freshness belongs to another root-approved request",
    )?;
    validate_window(challenge.issued_at_ms, challenge.expires_at_ms, now)?;
    let hash = challenge.hash()?;
    if verified.budget.objects.insert(hash.clone()) {
        let bytes = canonical(challenge)?.len();
        verified.budget.object_bytes.insert(hash.clone(), bytes);
        verified.budget.bytes += bytes;
        if verified.budget.objects.len() > MAX_PROOF_OBJECTS
            || verified.budget.bytes > MAX_PROOF_BYTES
        {
            return Err(AuthorizationError::BudgetExceeded);
        }
    }
    let mut seen = BTreeSet::new();
    for receipt in receipts {
        verified.budget.verify(receipt)?;
        let value = &receipt.payload;
        value.validate_currents()?;
        require(
            value.version == AUTH_VERSION
                && value.challenge_hash == hash
                && value.issued_at_ms >= challenge.issued_at_ms
                && value.issued_at_ms <= now
                && value.expires_at_ms <= challenge.expires_at_ms,
            "execution registration authority receipt changed",
        )?;
        validate_window(value.issued_at_ms, value.expires_at_ms, now)?;
        for current in &value.currents {
            let authority = match &current.domain {
                PolicyDomain::Account(id) => state.account(id).map(|account| &account.authority),
                PolicyDomain::Space(id) => state.space(id).map(|space| &space.authority),
            }
            .ok_or(AuthorizationError::PolicyConflict)?;
            require(
                receipt.issuer == authority.public_key
                    && authority.device == DeviceId::from_key(&receipt.issuer)
                    && authority.service == ServiceId::from_key(&receipt.issuer)
                    && state.head(&current.domain) == Some(current)
                    && challenge.policies.contains(current)
                    && seen.insert(current.domain.clone()),
                "execution registration authority receipt changed",
            )?;
        }
    }
    require(
        seen.len() == challenge.policies.len(),
        "execution registration is missing authority freshness",
    )
}

pub fn verify_execution_service_registration_commit(
    state: &VerifiedPolicySet,
    request: &ExecutionServiceRegistrationRequest,
    committed: &Committed<SpaceDocument>,
    freshness: Option<(&FreshnessChallenge, &[Signed<FreshnessReceipt>])>,
    local_node: &PublicKey,
    now: u64,
) -> Result<VerifiedExecutionServiceRegistrationRequest> {
    let _signature_scope = ScopedSignatureVerification::new();
    let mut verified = verify_execution_service_registration(state, request, local_node, now)?;
    if let Some((challenge, receipts)) = freshness {
        check_registration_freshness(
            state,
            request,
            challenge,
            receipts,
            local_node,
            now,
            &mut verified,
        )?;
    }
    require(
        committed.proposal == request.request.proposal
            && committed.commit.payload.nonce == request.request.nonce
            && committed.commit.payload.dependencies == verified.commit_heads,
        "execution registration commit belongs to another root-approved request",
    )?;
    verified.budget.verify(&committed.proposal)?;
    verified.budget.verify(&committed.commit)?;
    for receipt in &committed.receipts {
        verified.budget.verify(receipt)?;
    }
    let [
        JournalEntry::Resource(resource),
        JournalEntry::Grant(parent),
    ] = request.request.new_objects.as_slice()
    else {
        return Err(AuthorizationError::InvalidProof(
            "missing execution registration objects",
        ));
    };
    let mut candidate = state.clone();
    candidate.install_resource(resource.clone())?;
    candidate.install_grant(parent.clone(), now)?;
    candidate.install_space_committed(committed.clone(), now)?;
    verified.crypto_counts = SignatureVerification::current_counts();
    Ok(verified)
}

fn install_root(root: &str) -> Result<()> {
    require(
        root.starts_with('/')
            && root.len() <= 4096
            && !root.chars().any(char::is_control)
            && (root == "/" || !root.ends_with('/'))
            && !root.contains("//")
            && root.split('/').all(|part| part != "." && part != ".."),
        "execution install requires a canonical configured root",
    )
}

fn install_context(
    state: &VerifiedPolicySet,
    registration: &ExecutionServiceRegistrationRequest,
    committed: &Committed<SpaceDocument>,
    client: &PublicKey,
    target: &PublicKey,
    canonical_root: &str,
    now: u64,
) -> Result<(
    VerifiedExecutionServiceRegistrationRequest,
    ExecutionProofUsage,
    ObjectHash,
)> {
    install_root(canonical_root)?;
    let intent = &registration.intent.payload;
    let accepted = committed.commit.payload.accepted_at_ms;
    require(
        intent.target_node_public_key == *target
            && accepted <= now
            && state.commits.get(&committed.proposal.hash()?) == Some(&committed.commit),
        "execution install requires the exact accepted authority commit",
    )?;
    let historical = super::remote_resolution_history::at_heads(state, &intent.expected_heads)?;
    let registered = verify_execution_service_registration_commit(
        &historical,
        registration,
        committed,
        None,
        target,
        accepted,
    )?;
    let mut budget = Budget::default();
    let mut current = authorization(
        state,
        &intent.account,
        client,
        &intent.source_project,
        &intent.source_grant,
        target,
        now,
        &mut budget,
        true,
    )?;
    let [JournalEntry::Resource(_), JournalEntry::Grant(parent)] =
        registration.request.new_objects.as_slice()
    else {
        return Err(AuthorizationError::InvalidProof(
            "invalid execution install registration",
        ));
    };
    let parent_hash = parent.hash()?;
    verify_management_parent(
        state,
        &intent.account,
        client,
        &intent.execution_resource,
        &parent_hash,
        target,
        now,
        &mut budget,
    )?;
    current.budget = budget;
    let mut usage = registered.proof_usage()?;
    usage.record_budget(&current.budget)?;
    usage.record_metadata(&(registration, committed, canonical_root))?;
    usage.enforce_limits()?;
    Ok((current, usage, parent_hash))
}

pub fn issue_execution_service_install_challenge(
    state: &VerifiedPolicySet,
    registration: &ExecutionServiceRegistrationRequest,
    committed: &Committed<SpaceDocument>,
    account_client: &PublicKey,
    canonical_root: &str,
    signer: &Identity,
    now: u64,
) -> Result<Signed<FreshnessChallenge>> {
    let _signatures = ScopedSignatureVerification::new();
    let target = PublicKey::of(signer);
    let (current, mut usage, _) = install_context(
        state,
        registration,
        committed,
        account_client,
        &target,
        canonical_root,
        now,
    )?;
    let nonce = Nonce::random()?;
    let challenge = Signed::sign(
        FreshnessChallenge {
            version: AUTH_VERSION,
            stage: CheckStage::Admit,
            invocation_hash: registration.intent.hash()?,
            operation_id: format!("execution-service-install:{nonce}"),
            body_hash: ObjectHash::body(&(registration, committed, canonical_root))?,
            audience: Audience {
                service: ServiceId::from_key(&target),
                device: DeviceId::from_key(&target),
            },
            presenter: DeviceId::from_key(account_client),
            policies: current.expected_heads,
            nonce,
            issued_at_ms: now,
            expires_at_ms: now.saturating_add(EXECUTION_LEASE_MS).min(current.expires),
        },
        signer,
    )?;
    usage.record_signed(&challenge)?;
    finish_native_usage(&mut usage)?;
    Ok(challenge)
}

pub fn verify_execution_service_install(
    state: &VerifiedPolicySet,
    request: &ExecutionServiceInstallRequest,
    local_node: &PublicKey,
    configured_root: &str,
    now: u64,
) -> Result<VerifiedExecutionServiceInstall> {
    let _signatures = ScopedSignatureVerification::new();
    require(
        canonical(request)?.len() <= MAX_PROOF_BYTES,
        "execution install is too large",
    )?;
    request.intent.verify_signature()?;
    request.challenge.verify_signature()?;
    let intent = &request.intent.payload;
    let challenge = &request.challenge.payload;
    let registration = &request.registration.intent.payload;
    let (current, mut usage, parent) = install_context(
        state,
        &request.registration,
        &request.committed,
        &request.intent.issuer,
        local_node,
        configured_root,
        now,
    )?;
    validate_window(intent.issued_at_ms, intent.expires_at_ms, now)?;
    validate_window(challenge.issued_at_ms, challenge.expires_at_ms, now)?;
    require(
        intent.version == AUTH_VERSION
            && intent.account == registration.account
            && intent.registration_hash == ObjectHash::body(&request.registration)?
            && intent.committed_hash == ObjectHash::body(&request.committed)?
            && intent.target_node_public_key == *local_node
            && intent.canonical_root == configured_root
            && intent.expected_heads == current.expected_heads
            && intent.challenge_hash == request.challenge.hash()?
            && intent.nonce == challenge.nonce
            && intent.issued_at_ms >= challenge.issued_at_ms
            && intent.expires_at_ms <= challenge.expires_at_ms
            && request.challenge.issuer == *local_node
            && challenge.version == AUTH_VERSION
            && challenge.stage == CheckStage::Admit
            && challenge.invocation_hash == request.registration.intent.hash()?
            && challenge.operation_id == format!("execution-service-install:{}", challenge.nonce)
            && challenge.body_hash
                == ObjectHash::body(&(&request.registration, &request.committed, configured_root))?
            && challenge.audience
                == (Audience {
                    service: ServiceId::from_key(local_node),
                    device: DeviceId::from_key(local_node),
                })
            && challenge.presenter == DeviceId::from_key(&request.intent.issuer)
            && challenge.policies == current.expected_heads
            && challenge.expires_at_ms <= current.expires
            && challenge
                .expires_at_ms
                .saturating_sub(challenge.issued_at_ms)
                <= EXECUTION_LEASE_MS,
        "execution install differs from its current owner and target challenge",
    )?;
    usage.record_signed(&request.intent)?;
    usage.record_signed(&request.challenge)?;
    usage.record_metadata(request)?;
    let proof_counts = finish_native_usage(&mut usage)?;
    Ok(VerifiedExecutionServiceInstall {
        source_project: registration.source_project.clone(),
        execution_resource: registration.execution_resource.clone(),
        management_parent: parent,
        canonical_root: configured_root.to_owned(),
        nonce: challenge.nonce.clone(),
        request_hash: ObjectHash::body(request)?,
        expires_at_ms: intent.expires_at_ms,
        proof_counts,
    })
}

fn revalidate_selected(
    authorization: &VerifiedClientAuthorization,
    snapshot: &VerifiedPolicySnapshot,
    resource: &ResourceRef,
    signer: &PublicKey,
    now: u64,
) -> Result<VerifiedClientAuthorization> {
    require(
        authorization.scope.device_id == DeviceId::from_key(signer)
            && authorization.scope.space_id == resource.space
            && authorization.resources.len() == 1
            && authorization.resources[0].payload.resource == resource.resource,
        "execution operation changed its selected client or resource",
    )?;
    let bundle = ClientAuthorizationBundle {
        journal: Vec::new(),
        grant_leaf: authorization.grant_leaf.clone(),
    };
    let mut current = verify_client_policy_state_selected(
        snapshot.state().clone(),
        &bundle,
        snapshot.pins(),
        authorization.scope.account_id.clone(),
        resource.space.clone(),
        signer.clone(),
        Some(resource),
        now,
    )?;
    require(
        current.audience == authorization.audience,
        "execution operation changed its selected audience",
    )?;
    current.imported_snapshot = Some(snapshot.clone());
    current.client_policy_context = authorization.client_policy_context.clone();
    Ok(current)
}

fn finish_native_usage(usage: &mut ExecutionProofUsage) -> Result<RegistrationProofCounts> {
    if let Some(counts) = SignatureVerification::current_counts() {
        usage.record_actual_signature_counts(counts);
    }
    usage.enforce_limits()?;
    let counts = usage.counts();
    Ok(RegistrationProofCounts {
        objects: counts.unique_signed_objects,
        bytes: counts.authorization_bytes,
        signatures: counts.unique_signatures,
        max_depth: counts.max_chain_depth,
        actual_verifications: counts
            .actual
            .as_ref()
            .map_or(0, |value| value.actual_verifications),
        cache_hits: counts.actual.as_ref().map_or(0, |value| value.cache_hits),
    })
}

#[derive(Clone, Debug)]
#[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen)]
pub struct VerifiedExecutionServiceManagementSelection {
    snapshot: VerifiedPolicySnapshot,
    source_project: ResourceRef,
    source_scope: ClientScope,
    source_data_grant: ObjectHash,
    target_node: PublicKey,
    management_grant: ObjectHash,
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen)]
pub struct ExecutionServiceManagementPage {
    selection: Option<VerifiedExecutionServiceManagementSelection>,
    source_project: ResourceRef,
    target_node: PublicKey,
    next_cursor: Option<ObjectHash>,
    proof_counts: RegistrationProofCounts,
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen)]
impl ExecutionServiceManagementPage {
    pub fn selection(&self) -> Option<VerifiedExecutionServiceManagementSelection> {
        self.selection.clone()
    }
    #[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen(js_name = metadataJson))]
    pub fn metadata_json(&self) -> crate::Result<String> {
        json(&serde_json::json!({"source_project":self.source_project,
            "target_node_public_key":self.target_node,
            "management_grant":self.selection.as_ref().map(|value| &value.management_grant),
            "next_cursor":self.next_cursor}))
    }
    #[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen(js_name = proofUsageJson))]
    pub fn proof_usage_json(&self) -> crate::Result<String> {
        json(&self.proof_counts)
    }
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen(js_name = selectExecutionServiceManagement))]
pub fn select_execution_service_management(
    source: &VerifiedClientAuthorization,
    source_project_json: &str,
    target_node_public_key: &str,
    grant_cursor_json: &str,
    now_ms: f64,
) -> crate::Result<ExecutionServiceManagementPage> {
    let _signature_scope = ScopedSignatureVerification::new();
    let select = || -> Result<_> {
        let snapshot = require_confirmed_client_policy(source)?;
        let project: ResourceRef = parse(source_project_json)?;
        let target = PublicKey::parse(target_node_public_key)?;
        let cursor: Option<ObjectHash> = parse(grant_cursor_json)?;
        let now = bootstrap::timestamp(now_ms)?;
        let signer = snapshot
            .state()
            .find_device(&source.scope.device_id)?
            .payload
            .public_key
            .clone();
        let current = revalidate_selected(source, snapshot, &project, &signer, now)?;
        require(
            current.resources[0].payload.kind == ResourceKind::Project
                && current.resources[0].payload.host == current.audience.service,
            "execution management selection changed its source Project host",
        )?;
        let document = snapshot
            .state()
            .space(&project.space)
            .ok_or(AuthorizationError::PolicyConflict)?;
        let index = cursor.as_ref().map_or(0, |cursor| {
            document.grants.partition_point(|hash| hash <= cursor)
        });
        let candidate = document.grants.get(index);
        let next_cursor = if index.saturating_add(1) < document.grants.len() {
            candidate.cloned()
        } else {
            None
        };
        let mut usage = ExecutionProofUsage::default();
        usage.record_budget(&current.budget)?;
        usage.record_metadata(&(
            source_project_json,
            target_node_public_key,
            grant_cursor_json,
        ))?;
        let selection = if let Some(candidate) = candidate {
            let mut candidate_budget = Budget::default();
            let checked = authorization(
                snapshot.state(),
                &source.scope.account_id,
                &signer,
                &project,
                candidate,
                &target,
                now,
                &mut candidate_budget,
                false,
            );
            usage.record_budget(&candidate_budget)?;
            match checked {
                Ok(verified) => {
                    usage.record_budget(&verified.budget)?;
                    usage.record_chain_depth(verified.max_depth);
                    Some(VerifiedExecutionServiceManagementSelection {
                        snapshot: snapshot.clone(),
                        source_project: project.clone(),
                        source_scope: source.scope.clone(),
                        source_data_grant: source.grant_leaf.clone(),
                        target_node: target.clone(),
                        management_grant: candidate.clone(),
                    })
                }
                Err(error @ AuthorizationError::BudgetExceeded) => return Err(error),
                Err(_) => None,
            }
        } else {
            None
        };
        let proof_counts = finish_native_usage(&mut usage)?;
        Ok(ExecutionServiceManagementPage {
            selection,
            source_project: project,
            target_node: target,
            next_cursor,
            proof_counts,
        })
    };
    select().map_err(|error| error.to_string())
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen)]
pub struct PreparedExecutionServiceRegistration {
    request: ExecutionServiceRegistrationRequest,
    snapshot: VerifiedPolicySnapshot,
    owner_snapshot: VerifiedPolicySnapshot,
    owner_grant: ObjectHash,
    material: RecoveryMaterial,
    proof_counts: RegistrationProofCounts,
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen(js_name = prepareExecutionServiceRegistration))]
#[allow(clippy::too_many_arguments)]
pub fn prepare_execution_service_registration(
    source: &VerifiedClientAuthorization,
    management: &VerifiedExecutionServiceManagementSelection,
    owner: &VerifiedClientAuthorization,
    owner_material_json: &str,
    signer: &Identity,
    target_node_public_key: &str,
    now_ms: f64,
    expires_at_ms: f64,
) -> crate::Result<PreparedExecutionServiceRegistration> {
    let _signature_scope = ScopedSignatureVerification::new();
    let prepare = || -> Result<_> {
        let source_snapshot = require_confirmed_client_policy(source)?;
        let owner_snapshot = require_confirmed_client_policy(owner)?;
        let target = PublicKey::parse(target_node_public_key)?;
        let now = bootstrap::timestamp(now_ms)?;
        let expiry = bootstrap::timestamp(expires_at_ms)?;
        validate_window(now, expiry, now)?;
        require(
            management.source_scope == source.scope
                && management.source_data_grant == source.grant_leaf
                && management.target_node == target,
            "execution management selection belongs to another source, actor or Node",
        )?;
        let checked_source = merge_same_head_snapshots(source_snapshot, &management.snapshot)?;
        let material: RecoveryMaterial = parse(owner_material_json)?;
        let root = Identity::from_secret_hex(&material.root_secret_hex)
            .map_err(|_| AuthorizationError::InvalidProof("invalid execution owner root"))?;
        let recovery = Identity::from_secret_hex(&material.recovery_secret_hex).map_err(|_| {
            AuthorizationError::InvalidProof("invalid execution owner recovery key")
        })?;
        let account = material.scope.account_id.clone();
        let personal = SpaceId::personal(&account);
        require(
            material.format == "vibeface-account-recovery"
                && material.version == AUTH_VERSION
                && material.scope == owner.scope
                && owner.scope.space_id == personal
                && material.bundle.grant_leaf == owner.grant_leaf
                && source.scope.account_id == account
                && source.scope.device_id == DeviceId::from_key(&PublicKey::of(signer))
                && owner.scope.device_id == source.scope.device_id
                && material.pins.len() == 2
                && material.pins[0].domain < material.pins[1].domain
                && material
                    .pins
                    .iter()
                    .any(|pin| pin.domain == PolicyDomain::Account(account.clone()))
                && material
                    .pins
                    .iter()
                    .any(|pin| pin.domain == PolicyDomain::Space(personal.clone())),
            "execution registration requires this client's current PersonalSpace owner material",
        )?;
        let mut saved_pins = owner_snapshot.pins().to_vec();
        for pin in &material.pins {
            let saved = saved_pins
                .iter_mut()
                .find(|saved| saved.domain == pin.domain)
                .ok_or(AuthorizationError::InvalidProof(
                    "execution owner material changed saved trust domains",
                ))?;
            *saved = pin.clone();
        }
        let owner_snapshot = owner_snapshot.with_independent_pins(&saved_pins)?;
        let snapshot = merge_same_head_snapshots(&checked_source, &owner_snapshot)?;
        let state = snapshot.state();
        let current_account = state
            .account(&account)
            .ok_or(AuthorizationError::PolicyConflict)?;
        let current_personal = state
            .space(&personal)
            .ok_or(AuthorizationError::PolicyConflict)?;
        require(
            current_account.current_root == PublicKey::of(&root)
                && current_personal.current_root == PublicKey::of(&root)
                && current_account.recovery.threshold == 1
                && current_account.recovery.keys == [PublicKey::of(&recovery)]
                && current_personal.recovery == current_account.recovery
                && material.entry.node_public_key == current_account.authority.public_key,
            "execution owner's material is not its current public root and recovery policy",
        )?;
        let source_project = &management.source_project;
        let source_current = revalidate_selected(
            source,
            &snapshot,
            source_project,
            &PublicKey::of(signer),
            now,
        )?;
        let owner_project = owner
            .resources
            .first()
            .ok_or(AuthorizationError::InvalidProof(
                "missing execution owner Project",
            ))?;
        let owner_ref = ResourceRef {
            space: personal.clone(),
            resource: owner_project.payload.resource.clone(),
        };
        let owner_current =
            revalidate_selected(owner, &snapshot, &owner_ref, &PublicKey::of(signer), now)?;
        let verified = authorization(
            state,
            &account,
            &PublicKey::of(signer),
            source_project,
            &management.management_grant,
            &target,
            now,
            &mut Budget::default(),
            true,
        )?;
        let mut usage = ExecutionProofUsage::default();
        usage.record_budget(&source_current.budget)?;
        usage.record_budget(&owner_current.budget)?;
        usage.record_budget(&verified.budget)?;
        usage.record_chain_depth(verified.max_depth);
        usage.record_metadata(&owner_material_json)?;
        let execution_resource = ResourceRef {
            space: personal.clone(),
            resource: ResourceId::random()?,
        };
        let head = state
            .head(&PolicyDomain::Space(personal.clone()))
            .ok_or(AuthorizationError::PolicyConflict)?
            .clone();
        let resource = Signed::sign(
            ResourceRecord {
                version: AUTH_VERSION,
                space: personal.clone(),
                resource: execution_resource.resource.clone(),
                kind: ResourceKind::ExecutionService,
                host: ServiceId::from_key(&target),
                issued_under: head.clone(),
                nonce: Nonce::random()?,
            },
            &root,
        )?;
        let parent = Signed::sign(
            Grant {
                execution_source: None,
                version: AUTH_VERSION,
                owner_space: personal.clone(),
                scope: ResourceScope::Exact(execution_resource.clone()),
                grantee: Grantee::Actor(Actor::Account(account.clone())),
                actions: management_actions(),
                audience: Audience {
                    service: ServiceId::from_key(&target),
                    device: DeviceId::from_key(&target),
                },
                constraints: GrantConstraints::default(),
                parent: None,
                remaining_delegations: 1,
                issued_under: head.clone(),
                not_before_ms: now,
                expires_at_ms: verified.expires,
                nonce: Nonce::random()?,
            },
            &root,
        )?;
        let mut document = state
            .space(&personal)
            .ok_or(AuthorizationError::PolicyConflict)?
            .clone();
        document.previous = Some(head);
        document.sequence = document
            .sequence
            .checked_add(1)
            .ok_or(AuthorizationError::PolicyConflict)?;
        document.governance_grant = None;
        document.root_acceptance = None;
        document.recovery_approvals.clear();
        document.resources.push(resource.hash()?);
        document.resources.sort();
        document.grants.push(parent.hash()?);
        document.grants.sort();
        let request = governance::SpaceGovernanceRequest {
            proposal: Signed::sign(document, &root)?,
            new_objects: vec![
                JournalEntry::Resource(resource),
                JournalEntry::Grant(parent),
            ],
            nonce: Nonce::random()?,
        };
        let intent = Signed::sign(
            ExecutionServiceRegistrationIntent {
                version: AUTH_VERSION,
                account,
                source_project: source_project.clone(),
                source_grant: management.management_grant.clone(),
                execution_resource,
                target_node_public_key: target.clone(),
                expected_heads: verified.expected_heads,
                policy_request_hash: ObjectHash::body(&request)?,
                nonce: request.nonce.clone(),
                issued_at_ms: now,
                expires_at_ms: expiry,
            },
            signer,
        )?;
        let root_approval = Signed::sign(
            ExecutionServiceRootApproval {
                version: AUTH_VERSION,
                intent_hash: ObjectHash::body(&intent)?,
            },
            &root,
        )?;
        let request = ExecutionServiceRegistrationRequest {
            intent,
            root_approval,
            request,
        };
        let final_verified = verify_execution_service_registration(state, &request, &target, now)?;
        usage.record_budget(&final_verified.budget)?;
        usage.record_metadata(&request)?;
        let proof_counts = finish_native_usage(&mut usage)?;
        Ok(PreparedExecutionServiceRegistration {
            request,
            snapshot,
            owner_snapshot,
            owner_grant: owner.grant_leaf.clone(),
            material,
            proof_counts,
        })
    };
    prepare().map_err(|error| error.to_string())
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen)]
impl PreparedExecutionServiceRegistration {
    #[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen(js_name = proofUsageJson))]
    pub fn proof_usage_json(&self) -> crate::Result<String> {
        json(&self.proof_counts)
    }
    #[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen(js_name = requestJson))]
    pub fn request_json(&self) -> crate::Result<String> {
        json(&self.request)
    }
    #[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen(js_name = resourceId))]
    pub fn resource_id(&self) -> String {
        self.request
            .intent
            .payload
            .execution_resource
            .resource
            .to_string()
    }
    #[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen(js_name = spaceId))]
    pub fn space_id(&self) -> String {
        self.request
            .intent
            .payload
            .execution_resource
            .space
            .to_string()
    }
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen)]
pub struct VerifiedExecutionServiceRegistration {
    resource: ResourceRef,
    registration: ExecutionServiceRegistrationRequest,
    committed: Committed<SpaceDocument>,
    snapshot: VerifiedPolicySnapshot,
    authorization: VerifiedClientAuthorization,
    material_json: zeroize::Zeroizing<String>,
    proof_counts: RegistrationProofCounts,
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen(js_name = finishExecutionServiceRegistration))]
pub fn finish_execution_service_registration(
    prepared: &PreparedExecutionServiceRegistration,
    committed_json: &str,
    now_ms: f64,
) -> crate::Result<VerifiedExecutionServiceRegistration> {
    let _signature_scope = ScopedSignatureVerification::new();
    let finish = || -> Result<_> {
        let now = bootstrap::timestamp(now_ms)?;
        let committed: Committed<SpaceDocument> = parse(committed_json)?;
        let accepted = committed.commit.payload.accepted_at_ms;
        require(
            accepted <= now,
            "execution registration receipt is from the future",
        )?;
        let verified = verify_execution_service_registration_commit(
            prepared.snapshot.state(),
            &prepared.request,
            &committed,
            None,
            &prepared.request.intent.payload.target_node_public_key,
            accepted,
        )?;
        let [JournalEntry::Resource(_), JournalEntry::Grant(parent)] =
            prepared.request.request.new_objects.as_slice()
        else {
            return Err(AuthorizationError::InvalidProof(
                "missing prepared execution objects",
            ));
        };
        let mut delta = prepared.request.request.new_objects.clone();
        delta.push(JournalEntry::SpaceCommit(committed.clone()));
        let snapshot = prepared.snapshot.apply_delta(&delta)?;
        let mut new_pins = snapshot.pins().to_vec();
        for pin in &mut new_pins {
            pin.minimum_head = snapshot.state().head(&pin.domain).cloned();
        }
        let snapshot = snapshot.with_independent_pins(&new_pins)?;
        let owner_snapshot = prepared.owner_snapshot.apply_delta(&delta)?;
        let intent = &prepared.request.intent;
        let personal = &intent.payload.execution_resource.space;
        let bundle = ClientAuthorizationBundle {
            journal: Vec::new(),
            grant_leaf: parent.hash()?,
        };
        let mut authorization = verify_client_policy_state_selected(
            snapshot.state().clone(),
            &bundle,
            snapshot.pins(),
            intent.payload.account.clone(),
            personal.clone(),
            intent.issuer.clone(),
            Some(&intent.payload.execution_resource),
            accepted,
        )?;
        let mut usage = ExecutionProofUsage::default();
        usage.record_budget(&verified.budget)?;
        usage.record_budget(&authorization.budget)?;
        usage.record_chain_depth(verified.max_depth);
        usage.record_metadata(&committed)?;
        let (owner_state, mut owner_pins, owner_journal) = owner_snapshot.into_parts();
        for pin in &mut owner_pins {
            pin.minimum_head = owner_state.head(&pin.domain).cloned();
        }
        let mut material = RecoveryMaterial {
            format: prepared.material.format.clone(),
            version: prepared.material.version,
            root_secret_hex: prepared.material.root_secret_hex.clone(),
            recovery_secret_hex: prepared.material.recovery_secret_hex.clone(),
            scope: prepared.material.scope.clone(),
            pins: owner_pins,
            bundle: ClientAuthorizationBundle {
                journal: owner_journal,
                grant_leaf: prepared.owner_grant.clone(),
            },
            entry: prepared.material.entry.clone(),
            policy_checkpoint: None,
        };
        bootstrap::recovery::checkpoint_recovery_material_in_state(
            &mut material,
            &owner_state,
            accepted,
        )?;
        usage.merge(&bootstrap::recovery::recovery_material_usage(&material)?);
        let material_json =
            zeroize::Zeroizing::new(json(&material).map_err(|_| {
                AuthorizationError::InvalidProof("private material encoding failed")
            })?);
        require(
            material_json.len() <= MAX_PROOF_BYTES,
            "updated private material exceeds its bounded carrier",
        )?;
        usage.record_metadata(&material)?;
        usage.record_metadata(&prepared.request)?;
        let proof_counts = finish_native_usage(&mut usage)?;
        authorization.imported_snapshot = Some(snapshot.clone());
        authorization.client_policy_usage = Some(usage);
        Ok(VerifiedExecutionServiceRegistration {
            resource: intent.payload.execution_resource.clone(),
            registration: prepared.request.clone(),
            committed,
            snapshot,
            authorization,
            material_json,
            proof_counts,
        })
    };
    finish().map_err(|error| error.to_string())
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen)]
impl VerifiedExecutionServiceRegistration {
    #[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen(js_name = prepareInstall))]
    pub fn prepare_install(
        &self,
        challenge_json: &str,
        canonical_root: &str,
        signer: &Identity,
        now_ms: f64,
    ) -> crate::Result<String> {
        (|| -> Result<String> {
            let now = bootstrap::timestamp(now_ms)?;
            let challenge: Signed<FreshnessChallenge> = parse(challenge_json)?;
            let client = PublicKey::of(signer);
            require(
                self.authorization.scope.device_id == DeviceId::from_key(&client),
                "execution install changed its registered Account client",
            )?;
            let request = ExecutionServiceInstallRequest {
                registration: self.registration.clone(),
                committed: self.committed.clone(),
                intent: Signed::sign(
                    ExecutionServiceInstallIntent {
                        version: AUTH_VERSION,
                        account: self.registration.intent.payload.account.clone(),
                        registration_hash: ObjectHash::body(&self.registration)?,
                        committed_hash: ObjectHash::body(&self.committed)?,
                        target_node_public_key: self
                            .registration
                            .intent
                            .payload
                            .target_node_public_key
                            .clone(),
                        canonical_root: canonical_root.to_owned(),
                        expected_heads: challenge.payload.policies.clone(),
                        challenge_hash: challenge.hash()?,
                        nonce: challenge.payload.nonce.clone(),
                        issued_at_ms: now,
                        expires_at_ms: challenge.payload.expires_at_ms,
                    },
                    signer,
                )?,
                challenge,
            };
            verify_execution_service_install(
                self.snapshot.state(),
                &request,
                &self.registration.intent.payload.target_node_public_key,
                canonical_root,
                now,
            )?;
            serde_json::to_string(&request)
                .map_err(|_| AuthorizationError::InvalidProof("execution install encoding failed"))
        })()
        .map_err(|error| error.to_string())
    }

    #[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen(js_name = proofUsageJson))]
    pub fn proof_usage_json(&self) -> crate::Result<String> {
        json(&self.proof_counts)
    }
    #[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen(js_name = resourceId))]
    pub fn resource_id(&self) -> String {
        self.resource.resource.to_string()
    }
    #[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen(js_name = spaceId))]
    pub fn space_id(&self) -> String {
        self.resource.space.to_string()
    }
    pub fn authorization(&self) -> VerifiedClientAuthorization {
        self.authorization.clone()
    }
    #[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen(js_name = pinsJson))]
    pub fn pins_json(&self) -> crate::Result<String> {
        json(&self.authorization.policy_pins)
    }
    #[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen(js_name = scopeJson))]
    pub fn scope_json(&self) -> crate::Result<String> {
        json(&self.authorization.scope)
    }
    #[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen(js_name = recoveryMaterialJson))]
    pub fn recovery_material_json(&self) -> String {
        self.material_json.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    const NOW: f64 = 10_000.0;
    fn key(byte: u8) -> Identity {
        Identity::from_secret_hex(&hex::encode([byte; 32])).unwrap()
    }
    fn confirmed_authorization(
        state: &VerifiedPolicySet,
        pins: &[PolicyPin],
        bundle: &ClientAuthorizationBundle,
        resource: &ResourceRef,
    ) -> VerifiedClientAuthorization {
        use client_policy_import::*;
        let own = key(3);
        let node = key(4);
        let audience = state
            .grant(&bundle.grant_leaf)
            .unwrap()
            .payload
            .audience
            .clone();
        let account = match &state
            .find_device(&DeviceId::from_key(&PublicKey::of(&own)))
            .unwrap()
            .payload
            .subject
        {
            Actor::Account(account) => account.clone(),
            _ => unreachable!(),
        };
        let selection = AuthorizationSelection {
            resource: resource.clone(),
            audience,
            grant_leaf: bundle.grant_leaf.clone(),
        };
        let request_json = build_client_policy_read_request(
            account.as_str(),
            resource.space.as_str(),
            &own,
            &node.public_key_hex(),
            &json(&pins).unwrap(),
            &json(&Some(&selection)).unwrap(),
            NOW,
            NOW + 60_000.0,
        )
        .unwrap();
        let request: Signed<ClientPolicyReadRequest> = parse(&request_json).unwrap();
        let current = verify_client_policy_read(
            state,
            &request,
            &PublicKey::of(&node),
            &PublicKey::of(&own),
            NOW as u64,
        )
        .unwrap();
        let snapshot = Signed::sign(
            ClientPolicySnapshot {
                version: AUTH_VERSION,
                request_hash: request.hash().unwrap(),
                snapshot_id: Nonce::random().unwrap(),
                saved_pins_hash: client_policy_saved_pins_hash(pins).unwrap(),
                target_heads: current.heads().to_vec(),
                selection: selection.clone(),
                source_node_public_key: PublicKey::of(&node),
                issued_at_ms: NOW as u64,
                expires_at_ms: NOW as u64 + 60_000,
            },
            &node,
        )
        .unwrap();
        let mut import = begin_client_policy_import(
            &json(&pins).unwrap(),
            &request_json,
            &json(&snapshot).unwrap(),
            &own,
            &node.public_key_hex(),
            NOW,
        )
        .unwrap();
        let mut previous = snapshot.hash().unwrap();
        let mut request_cursor = None;
        for (index, entry) in bundle.journal.iter().enumerate() {
            let complete = index + 1 == bundle.journal.len();
            let page = Signed::sign(
                ClientPolicyPage {
                    version: AUTH_VERSION,
                    request_hash: request.hash().unwrap(),
                    snapshot_id: snapshot.payload.snapshot_id.clone(),
                    target_heads_hash: client_policy_target_heads_hash(current.heads()).unwrap(),
                    page_index: index as u64,
                    request_cursor: request_cursor.clone(),
                    previous_page_hash: previous,
                    entry: Some(entry.clone()),
                    next_cursor: if complete {
                        None
                    } else {
                        Some(ObjectHash::body(&("native-test-cursor", index)).unwrap())
                    },
                    complete,
                },
                &node,
            )
            .unwrap();
            previous = page.hash().unwrap();
            request_cursor = page.payload.next_cursor.clone();
            import.append_page(&json(&page).unwrap(), NOW).unwrap();
        }
        let witness = Signed::sign(
            ClientPolicyWitness {
                version: AUTH_VERSION,
                request_hash: request.hash().unwrap(),
                snapshot_id: snapshot.payload.snapshot_id.clone(),
                target_heads: current.heads().to_vec(),
                final_page_hash: previous,
                page_count: bundle.journal.len() as u64,
                entry_count: bundle.journal.len() as u64,
                snapshot_hash: snapshot.hash().unwrap(),
                source_node_public_key: PublicKey::of(&node),
                issued_at_ms: NOW as u64,
                expires_at_ms: NOW as u64 + 60_000,
            },
            &node,
        )
        .unwrap();
        let authorization = import
            .finish(&json(&witness).unwrap(), &json(&selection).unwrap(), NOW)
            .unwrap();
        let confirmation_request_json =
            build_client_policy_auth_confirm_request(&authorization, &own, NOW, NOW + 30_000.0)
                .unwrap();
        let confirmation_request: Signed<ClientPolicyAuthConfirmRequest> =
            parse(&confirmation_request_json).unwrap();
        verify_client_policy_auth_confirm(
            state,
            &request,
            &snapshot,
            &witness,
            &confirmation_request,
            &PublicKey::of(&node),
            &PublicKey::of(&own),
            NOW as u64,
        )
        .unwrap();
        let confirmation = Signed::sign(
            ClientPolicyAuthConfirmation {
                version: AUTH_VERSION,
                context: confirmation_request.payload.context.clone(),
                confirmation_request_hash: confirmation_request.hash().unwrap(),
                selection,
                target_heads: current.heads().to_vec(),
                device_public_key: PublicKey::of(&own),
                source_node_public_key: PublicKey::of(&node),
                accepted_at_ms: NOW as u64,
            },
            &node,
        )
        .unwrap();
        confirm_client_policy_authorization(
            &authorization,
            &confirmation_request_json,
            &json(&confirmation).unwrap(),
            &node.public_key_hex(),
            NOW,
        )
        .unwrap()
    }
    fn management_selection(
        source: &VerifiedClientAuthorization,
        project: &ResourceRef,
    ) -> VerifiedExecutionServiceManagementSelection {
        management_selection_for(source, project, &key(4))
    }
    fn management_selection_for(
        source: &VerifiedClientAuthorization,
        project: &ResourceRef,
        target: &Identity,
    ) -> VerifiedExecutionServiceManagementSelection {
        let mut cursor = "null".to_owned();
        loop {
            let page = select_execution_service_management(
                source,
                &json(project).unwrap(),
                &target.public_key_hex(),
                &cursor,
                NOW,
            )
            .unwrap();
            if let Some(selection) = page.selection() {
                return selection;
            }
            let metadata: serde_json::Value =
                serde_json::from_str(&page.metadata_json().unwrap()).unwrap();
            assert!(
                !metadata["next_cursor"].is_null(),
                "missing execution management grant"
            );
            cursor = metadata["next_cursor"].to_string();
        }
    }
    fn registration() -> (
        VerifiedPolicySet,
        PreparedExecutionServiceRegistration,
        VerifiedClientAuthorization,
    ) {
        registration_for_node(false)
    }
    fn registration_for_node(
        separate_executor: bool,
    ) -> (
        VerifiedPolicySet,
        PreparedExecutionServiceRegistration,
        VerifiedClientAuthorization,
    ) {
        let nonce = Nonce::from_bytes([9; 32]).to_string();
        let claim = bootstrap::verify_node_claim(
            &bootstrap::create_node_claim(&key(4), &nonce, NOW, NOW + 100_000.0).unwrap(),
            &key(4).public_key_hex(),
            &nonce,
            NOW,
        )
        .unwrap();
        let prepared = bootstrap::prepare_account_creation(
            &key(1),
            &key(2),
            &key(3),
            &claim,
            1_000_000.0,
            NOW,
        )
        .unwrap();
        let possession = bootstrap::sign_node_possession(
            &prepared.node_request_json().unwrap(),
            &key(4),
            &nonce,
            NOW,
        )
        .unwrap();
        let created =
            bootstrap::finish_account_creation(&prepared, &key(1), &possession, NOW).unwrap();
        let material = bootstrap::recovery::build_account_recovery_material(
            &created,
            &key(1),
            &key(2),
            "https://node.example",
            NOW,
        )
        .unwrap();
        let pins: Vec<PolicyPin> = parse(&created.pins_json().unwrap()).unwrap();
        let mut bundle: ClientAuthorizationBundle = parse(&created.bundle_json().unwrap()).unwrap();
        let mut state = replay_policies(&pins, &bundle.journal).unwrap();
        let target = if separate_executor { key(5) } else { key(4) };
        if separate_executor {
            let account: AccountId = parse::<ClientScope>(&created.scope_json().unwrap())
                .unwrap()
                .account_id;
            let mut certificate = DeviceCertificate {
                version: AUTH_VERSION,
                owner: PolicyDomain::Account(account.clone()),
                subject: Actor::Service(ServiceId::from_key(&PublicKey::of(&target))),
                device: DeviceId::from_key(&PublicKey::of(&target)),
                public_key: PublicKey::of(&target),
                uses: vec![DeviceUse::Noise, DeviceUse::ServiceInvocation],
                possession: None,
                enrollment_grant: None,
                issued_under: state
                    .head(&PolicyDomain::Account(account.clone()))
                    .unwrap()
                    .clone(),
                nonce: Nonce::random().unwrap(),
                not_before_ms: 0,
                expires_at_ms: 1_000_000,
            };
            certificate.prove_possession(&target).unwrap();
            let certificate = Signed::sign(certificate, &key(1)).unwrap();
            let hash = state
                .install_device(certificate.clone(), NOW as u64)
                .unwrap();
            let mut document = state.account(&account).unwrap().clone();
            document.previous = state.head(&PolicyDomain::Account(account.clone())).cloned();
            document.sequence += 1;
            document.devices.push(hash);
            document.devices.sort();
            let document = Signed::sign(document, &key(1)).unwrap();
            state.install_account(document.clone()).unwrap();
            bundle.journal.extend([
                JournalEntry::Device(certificate),
                JournalEntry::Account(document),
            ]);
        }
        let source = bundle
            .journal
            .iter()
            .find_map(|entry| match entry {
                JournalEntry::Resource(record) => Some(ResourceRef {
                    space: record.payload.space.clone(),
                    resource: record.payload.resource.clone(),
                }),
                _ => None,
            })
            .unwrap();
        let authorization = confirmed_authorization(&state, &pins, &bundle, &source);
        let management = management_selection_for(&authorization, &source, &target);
        let registration = prepare_execution_service_registration(
            &authorization,
            &management,
            &authorization,
            &material,
            &key(3),
            &target.public_key_hex(),
            NOW,
            NOW + 60_000.0,
        )
        .unwrap();
        (state, registration, authorization)
    }

    fn committed(
        state: &VerifiedPolicySet,
        prepared: &PreparedExecutionServiceRegistration,
    ) -> Committed<SpaceDocument> {
        let verified = verify_execution_service_registration(
            state,
            &prepared.request,
            &prepared.request.intent.payload.target_node_public_key,
            NOW as u64,
        )
        .unwrap();
        let challenge = Signed::sign(
            PolicyCommit {
                version: AUTH_VERSION,
                previous: state
                    .head(&PolicyDomain::Space(
                        prepared
                            .request
                            .intent
                            .payload
                            .execution_resource
                            .space
                            .clone(),
                    ))
                    .unwrap()
                    .clone(),
                proposal_hash: prepared.request.request.proposal.hash().unwrap(),
                accepted_at_ms: 0,
                issued_at_ms: NOW as u64,
                expires_at_ms: NOW as u64 + EXECUTION_LEASE_MS,
                nonce: prepared.request.request.nonce.clone(),
                dependencies: verified.commit_heads,
            },
            &key(4),
        )
        .unwrap();
        let receipts = vec![
            Signed::sign(
                FreshnessReceipt {
                    version: AUTH_VERSION,
                    currents: challenge.payload.dependencies.clone(),
                    challenge_hash: challenge.challenge_hash().unwrap(),
                    issued_at_ms: NOW as u64,
                    expires_at_ms: challenge.payload.expires_at_ms,
                },
                &key(4),
            )
            .unwrap(),
        ];
        let mut accepted = challenge.payload;
        accepted.accepted_at_ms = NOW as u64;
        Committed {
            proposal: prepared.request.request.proposal.clone(),
            commit: Signed::sign(accepted, &key(4)).unwrap(),
            receipts,
        }
    }

    #[test]
    fn separate_executor_installs_only_the_committed_resource_and_challenged_root() {
        let (mut state, prepared, _) = registration_for_node(true);
        let source = &prepared.request.intent.payload.source_project;
        assert_eq!(
            state
                .resource(&source.space, &source.resource)
                .unwrap()
                .payload
                .host,
            ServiceId::from_key(&PublicKey::of(&key(4)))
        );
        assert_eq!(
            prepared.request.intent.payload.target_node_public_key,
            PublicKey::of(&key(5))
        );
        assert!(
            !state
                .resources
                .values()
                .any(|record| record.payload.kind == ResourceKind::Project
                    && record.payload.host == ServiceId::from_key(&PublicKey::of(&key(5))))
        );
        let commit = committed(&state, &prepared);
        let JournalEntry::Grant(parent) = &prepared.request.request.new_objects[1] else {
            unreachable!()
        };
        assert!(parent.payload.actions.contains(&Action::ExecutionCommand));
        assert!(parent.payload.actions.contains(&Action::ResourceManage));
        let registered =
            finish_execution_service_registration(&prepared, &json(&commit).unwrap(), NOW).unwrap();
        let root = "/configured/executor";
        assert!(
            issue_execution_service_install_challenge(
                &state,
                &prepared.request,
                &commit,
                &PublicKey::of(&key(3)),
                root,
                &key(5),
                NOW as u64
            )
            .is_err()
        );
        for entry in &prepared.request.request.new_objects {
            match entry {
                JournalEntry::Resource(resource) => {
                    state.install_resource(resource.clone()).unwrap();
                }
                JournalEntry::Grant(grant) => {
                    state.install_grant(grant.clone(), NOW as u64).unwrap();
                }
                _ => unreachable!(),
            }
        }
        state
            .install_space_committed(commit.clone(), NOW as u64)
            .unwrap();
        let before = state
            .head(&PolicyDomain::Space(source.space.clone()))
            .unwrap()
            .clone();
        let challenge = issue_execution_service_install_challenge(
            &state,
            &prepared.request,
            &commit,
            &PublicKey::of(&key(3)),
            root,
            &key(5),
            NOW as u64,
        )
        .unwrap();
        let request: ExecutionServiceInstallRequest = parse(
            &registered
                .prepare_install(&json(&challenge).unwrap(), root, &key(3), NOW)
                .unwrap(),
        )
        .unwrap();
        let verify = |request: &ExecutionServiceInstallRequest, root: &str| {
            verify_execution_service_install(
                &state,
                request,
                &PublicKey::of(&key(5)),
                root,
                NOW as u64,
            )
        };
        let verified = verify(&request, root).unwrap();
        assert_eq!(verified.source_project(), source);
        assert_eq!(
            verified.execution_resource(),
            &prepared.request.intent.payload.execution_resource
        );
        assert_eq!(verified.canonical_root(), root);
        assert_eq!(verified.nonce(), &challenge.payload.nonce);
        assert_eq!(
            state.head(&PolicyDomain::Space(source.space.clone())),
            Some(&before)
        );
        assert!(verify(&request, "/configured/other").is_err());
        let mut changed = request.clone();
        changed.intent.payload.canonical_root = "/configured/other".into();
        changed.intent = Signed::sign(changed.intent.payload, &key(3)).unwrap();
        assert!(verify(&changed, "/configured/other").is_err());
        for signer in [key(2), key(5)] {
            let mut changed = request.clone();
            changed.intent = Signed::sign(changed.intent.payload, &signer).unwrap();
            assert!(verify(&changed, root).is_err());
        }
        let mut changed = request.clone();
        changed.challenge = Signed::sign(changed.challenge.payload, &key(4)).unwrap();
        changed.intent.payload.challenge_hash = changed.challenge.hash().unwrap();
        changed.intent = Signed::sign(changed.intent.payload, &key(3)).unwrap();
        assert!(verify(&changed, root).is_err());
        assert!(
            verify_execution_service_install(
                &state,
                &request,
                &PublicKey::of(&key(5)),
                root,
                NOW as u64 + EXECUTION_LEASE_MS
            )
            .is_err()
        );
        assert!(
            issue_execution_service_install_challenge(
                &state,
                &prepared.request,
                &commit,
                &PublicKey::of(&key(3)),
                "/configured/../executor",
                &key(5),
                NOW as u64
            )
            .is_err()
        );
        let mut revoked = state.clone();
        let account = &prepared.request.intent.payload.account;
        let mut document = revoked.account(account).unwrap().clone();
        document.previous = revoked
            .head(&PolicyDomain::Account(account.clone()))
            .cloned();
        document.sequence += 1;
        document
            .revoked_devices
            .push(DeviceId::from_key(&PublicKey::of(&key(5))));
        document.revoked_devices.sort();
        document.devices.retain(|hash| {
            revoked.device(hash).unwrap().payload.public_key != PublicKey::of(&key(5))
        });
        revoked
            .install_account(Signed::sign(document, &key(1)).unwrap())
            .unwrap();
        assert!(
            verify_execution_service_install(
                &revoked,
                &request,
                &PublicKey::of(&key(5)),
                root,
                NOW as u64
            )
            .is_err()
        );
    }

    #[test]
    fn registration_rejects_legacy_owner_without_genuine_public_import() {
        let (_, prepared, authorization) = registration();
        let management = management_selection(
            &authorization,
            &prepared.request.intent.payload.source_project,
        );
        let mut legacy = authorization.clone();
        legacy.imported_snapshot = None;
        let result = prepare_execution_service_registration(
            &authorization,
            &management,
            &legacy,
            &json(&prepared.material).unwrap(),
            &key(3),
            &key(4).public_key_hex(),
            NOW,
            NOW + 60_000.0,
        );
        assert!(matches!(result,Err(error) if error.contains("genuine policy snapshot")));
    }

    #[test]
    fn registration_freshness_batches_require_exact_current_authority_coverage() {
        let (state, prepared, _) = registration();
        let local_node = PublicKey::of(&key(4));
        let main = verify_execution_service_registration(
            &state,
            &prepared.request,
            &local_node,
            NOW as u64,
        )
        .unwrap();
        let challenge = execution_service_registration_challenge(
            &state,
            &prepared.request,
            &local_node,
            NOW as u64,
        )
        .unwrap();
        assert!(challenge.policies.len() > 1);
        let sign = |currents: Vec<PolicyStamp>| {
            Signed::sign(
                FreshnessReceipt {
                    version: AUTH_VERSION,
                    currents,
                    challenge_hash: challenge.hash().unwrap(),
                    issued_at_ms: NOW as u64,
                    expires_at_ms: challenge.expires_at_ms,
                },
                &key(4),
            )
            .unwrap()
        };
        let verify = |receipts: &[Signed<FreshnessReceipt>]| {
            verify_execution_service_registration_freshness(
                &state,
                &prepared.request,
                &challenge,
                receipts,
                &local_node,
                NOW as u64,
            )
        };
        let receipt = sign(challenge.policies.clone());
        let verified = verify(std::slice::from_ref(&receipt)).unwrap();
        assert_eq!(
            verified.proof_counts().objects,
            main.proof_counts().objects + 2
        );
        assert_eq!(
            verified.proof_counts().signatures,
            main.proof_counts().signatures + 1
        );

        let mut duplicate = challenge.policies.clone();
        duplicate.insert(1, duplicate[0].clone());
        let mut unsorted = challenge.policies.clone();
        unsorted.reverse();
        let mut missing = challenge.policies.clone();
        missing.pop();
        let mut extra = challenge.policies.clone();
        let mut foreign = extra[0].clone();
        foreign.domain = PolicyDomain::Account(AccountId::from_root(&PublicKey::of(&key(9))));
        extra.push(foreign);
        extra.sort_by(|left, right| left.domain.cmp(&right.domain));
        let mut stale = challenge.policies.clone();
        stale[0].sequence += 1;
        for currents in [vec![], duplicate, unsorted, missing, extra, stale] {
            assert!(verify(&[sign(currents)]).is_err());
        }
        assert!(verify(&[]).is_err());
        assert!(verify(&[receipt.clone(), sign(vec![challenge.policies[0].clone()]),]).is_err());
        let foreign_authority = Signed::sign(receipt.payload, &key(9)).unwrap();
        assert!(verify(&[foreign_authority]).is_err());
    }

    #[test]
    fn registration_commit_counts_root_source_and_receipt_objects_in_one_budget() {
        let (state, prepared, _) = registration();
        let main = verify_execution_service_registration(
            &state,
            &prepared.request,
            &PublicKey::of(&key(4)),
            NOW as u64,
        )
        .unwrap();
        let commit = committed(&state, &prepared);
        let final_proof = verify_execution_service_registration_commit(
            &state,
            &prepared.request,
            &commit,
            None,
            &PublicKey::of(&key(4)),
            NOW as u64,
        )
        .unwrap();
        assert_eq!(
            final_proof.proof_counts().objects,
            main.proof_counts().objects + 1 + commit.receipts.len()
        );
        assert_eq!(
            final_proof.proof_counts().signatures,
            main.proof_counts().signatures + 1 + commit.receipts.len()
        );
        let mut at_limit = final_proof.budget;
        let mut serial = 0;
        while at_limit.objects.len() < MAX_PROOF_OBJECTS
            && at_limit.signatures.len() + at_limit.external_signatures.len() < MAX_PROOF_SIGNATURES
        {
            let approval = Signed::sign(
                ExecutionServiceRootApproval {
                    version: AUTH_VERSION,
                    intent_hash: ObjectHash::body(&serial).unwrap(),
                },
                &key(1),
            )
            .unwrap();
            at_limit.verify(&approval).unwrap();
            serial += 1;
        }
        assert_eq!(
            at_limit.signatures.len() + at_limit.external_signatures.len(),
            MAX_PROOF_SIGNATURES
        );
        let overflow = Signed::sign(
            ExecutionServiceRootApproval {
                version: AUTH_VERSION,
                intent_hash: ObjectHash::body(&serial).unwrap(),
            },
            &key(1),
        )
        .unwrap();
        assert_eq!(
            at_limit.verify(&overflow),
            Err(AuthorizationError::BudgetExceeded)
        );
    }

    #[test]
    fn registration_cost_gate_accepts_exact_byte_limit_and_rejects_one_more_byte() {
        let (state, prepared, _) = registration();
        let main = verify_execution_service_registration(
            &state,
            &prepared.request,
            &PublicKey::of(&key(4)),
            NOW as u64,
        )
        .unwrap();
        let mut challenge = execution_service_registration_challenge(
            &state,
            &prepared.request,
            &PublicKey::of(&key(4)),
            NOW as u64,
        )
        .unwrap();
        challenge.operation_id.clear();
        let empty = Signed::sign(challenge.clone(), &key(4)).unwrap();
        challenge.operation_id =
            "x".repeat(MAX_PROOF_BYTES - main.budget.bytes - canonical(&empty).unwrap().len());
        let boundary = Signed::sign(challenge.clone(), &key(4)).unwrap();
        let mut at_limit = main.budget.clone();
        at_limit.verify(&boundary).unwrap();
        assert_eq!(at_limit.bytes, MAX_PROOF_BYTES);
        challenge.operation_id.push('x');
        let oversized = Signed::sign(challenge, &key(4)).unwrap();
        assert_eq!(
            main.budget.clone().verify(&oversized),
            Err(AuthorizationError::BudgetExceeded)
        );
    }
}
