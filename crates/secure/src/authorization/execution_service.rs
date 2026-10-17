//! Execution services require their physical owner's current personal root.

use super::{bootstrap::recovery::RecoveryMaterial, governance, *};
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
pub struct ExecutionServiceSelection {
    pub source_project: ResourceRef,
    pub execution_resource: ResourceRef,
    pub authorization: ClientAuthorizationBundle,
    pub pins: Vec<PolicyPin>,
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
    pub fn proof_counts(&self) -> RegistrationProofCounts {
        RegistrationProofCounts {
            objects: self.budget.objects.len(),
            bytes: self.budget.bytes,
            signatures: self.budget.signatures.len() + self.budget.external_signatures.len(),
            max_depth: self.max_depth,
            actual_verifications: self
                .crypto_counts
                .as_ref()
                .map_or(0, |counts| counts.actual_verifications as usize),
            cache_hits: self
                .crypto_counts
                .as_ref()
                .map_or(0, |counts| counts.cache_hits as usize),
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
        Action::ExecutionSteer,
        Action::ExecutionInterrupt,
        Action::ExecutionRespondApproval,
        Action::ExecutionSubmit,
        Action::ExecutionResolve,
        Action::OperationStatusRead,
        Action::OperationResolve,
        Action::GrantIssue,
        Action::GrantRevoke,
    ]
}

fn authorization(
    state: &VerifiedPolicySet,
    account: &AccountId,
    signer: &PublicKey,
    source: &ResourceRef,
    source_grant: &ObjectHash,
    node: &PublicKey,
    now: u64,
    budget: &mut Budget,
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
    for space in [&personal, &source.space] {
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
    require(
        record.payload.kind == ResourceKind::Project
            && record.payload.host == ServiceId::from_key(node),
        "execution source must be a project hosted by the selected node",
    )?;
    budget.verify(record)?;
    state.validate_certificate(target, now, budget, &mut dependencies, &mut BTreeSet::new())?;
    dependencies.extend(budget.dependencies.clone());
    state.dependency(
        &PolicyDomain::Space(source.space.clone()),
        &mut dependencies,
    )?;
    state.dependency(&PolicyDomain::Space(personal.clone()), &mut dependencies)?;
    state.expand_authorities(now, budget, &mut dependencies)?;
    let all_dependencies = budget.dependencies.clone();
    budget.dependencies.clear();
    let mut commit_heads = BTreeMap::new();
    state.dependency(&PolicyDomain::Space(personal), &mut commit_heads)?;
    state.expand_authorities(now, budget, &mut commit_heads)?;
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
) -> Result<()> {
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
    )?;
    Ok(())
}

fn verify_management_parent(
    state: &VerifiedPolicySet,
    account: &AccountId,
    signer: &PublicKey,
    resource: &ResourceRef,
    parent: &ObjectHash,
    node: &PublicKey,
    now: u64,
) -> Result<()> {
    let personal = SpaceId::personal(account);
    require(
        resource.space == personal,
        "execution service must belong to the physical owner's personal space",
    )?;
    let mut budget = Budget::default();
    let mut dependencies = BTreeMap::new();
    let client = state.find_device(&DeviceId::from_key(signer))?;
    require(
        client.payload.owner == PolicyDomain::Account(account.clone())
            && client.payload.uses.contains(&DeviceUse::Noise)
            && client.payload.uses.contains(&DeviceUse::AccountInvocation)
            && state.actor_for_signer(signer, now, &mut budget, &mut dependencies)?
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
    state.validate_certificate(
        target,
        now,
        &mut budget,
        &mut dependencies,
        &mut BTreeSet::new(),
    )?;
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
    state.verify_grant_path(&path, now, &mut budget, &mut dependencies, true)?;
    Ok(())
}

pub fn project_execution_service_selection(
    state: &VerifiedPolicySet,
    pins: &[PolicyPin],
    journal: &[JournalEntry],
    account: &AccountId,
    signer: &PublicKey,
    source: &ResourceRef,
    resource: &ResourceRef,
    parent: &ObjectHash,
    node: &PublicKey,
    now: u64,
) -> Result<ExecutionServiceSelection> {
    let _signature_scope = ScopedSignatureVerification::new();
    verify_management_parent(state, account, signer, resource, parent, node, now)?;
    let authorization = state.client_authorization_projection(
        pins,
        journal,
        account,
        &resource.space,
        signer,
        parent,
        now,
    )?;
    Ok(ExecutionServiceSelection {
        source_project: source.clone(),
        execution_resource: resource.clone(),
        pins: projection_pins(pins, &authorization),
        authorization,
    })
}

/// The authenticated physical Node supplies the source mapping. Pins remain independent.
#[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen(js_name = verifyExecutionServiceSelection))]
pub fn verify_execution_service_selection(
    selection_json: &str,
    pinned_roots_json: &str,
    expected_source_project_json: &str,
    expected_execution_resource_json: &str,
    expected_account: &str,
    expected_device_public_key: &str,
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
        let pins: Vec<PolicyPin> = parse(pinned_roots_json)?;
        let account = AccountId::parse(expected_account)?;
        let signer = PublicKey::parse(expected_device_public_key)?;
        let node = PublicKey::parse(expected_target_node_public_key)?;
        let now = bootstrap::timestamp(now_ms)?;
        let state = replay_policies(&pins, &selection.authorization.journal)?;
        verify_management_parent(
            &state,
            &account,
            &signer,
            &resource,
            &selection.authorization.grant_leaf,
            &node,
            now,
        )?;
        verify_client_authorization(
            &serde_json::to_string(&selection.authorization).map_err(|_| {
                AuthorizationError::InvalidProof("execution selection encoding failed")
            })?,
            pinned_roots_json,
            expected_account,
            resource.space.as_str(),
            expected_device_public_key,
            now_ms,
        )
        .map_err(|_| AuthorizationError::InvalidProof("invalid execution selection authorization"))
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
    require(
        receipts.len() == challenge.policies.len(),
        "execution registration is missing authority freshness",
    )?;
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
        let authority = match &value.current.domain {
            PolicyDomain::Account(id) => state.account(id).map(|account| &account.authority),
            PolicyDomain::Space(id) => state.space(id).map(|space| &space.authority),
        }
        .ok_or(AuthorizationError::PolicyConflict)?;
        require(
            receipt.issuer == authority.public_key
                && value.version == AUTH_VERSION
                && value.challenge_hash == hash
                && challenge.policies.contains(&value.current)
                && seen.insert(value.current.domain.clone())
                && value.issued_at_ms >= challenge.issued_at_ms
                && value.issued_at_ms <= now
                && value.expires_at_ms <= challenge.expires_at_ms,
            "execution registration authority receipt changed",
        )?;
        validate_window(value.issued_at_ms, value.expires_at_ms, now)?;
    }
    Ok(())
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

fn merged_context(
    account: &AccountId,
    owner_pins: Vec<PolicyPin>,
    owner_bundle: ClientAuthorizationBundle,
    pins: Vec<PolicyPin>,
    bundle: ClientAuthorizationBundle,
) -> Result<(Vec<PolicyPin>, ClientAuthorizationBundle, VerifiedPolicySet)> {
    let saved = replay_policies(&owner_pins, &owner_bundle.journal)?;
    let source = replay_policies(&pins, &bundle.journal)?;
    let domain = PolicyDomain::Account(account.clone());
    let saved_head = saved
        .head(&domain)
        .ok_or(AuthorizationError::PolicyConflict)?;
    let source_head = source
        .head(&domain)
        .ok_or(AuthorizationError::PolicyConflict)?;
    let saved_first = source.roots.contains_key(saved_head);
    require(
        saved_first || saved.roots.contains_key(source_head),
        "source and personal account histories conflict",
    )?;
    let mut combined: BTreeMap<_, _> = owner_pins
        .iter()
        .map(|pin| (pin.domain.clone(), pin.clone()))
        .collect();
    for pin in pins {
        if let Some(previous) = combined.get(&pin.domain) {
            require(
                previous.genesis_root == pin.genesis_root,
                "source changed saved personal trust",
            )?;
        }
        combined.insert(pin.domain.clone(), pin);
    }
    let mut journal = Vec::new();
    let mut seen = BTreeSet::new();
    let histories = if saved_first {
        [&owner_bundle.journal, &bundle.journal]
    } else {
        [&bundle.journal, &owner_bundle.journal]
    };
    for history in histories {
        for entry in history {
            if seen.insert(ObjectHash::body(entry)?) {
                journal.push(entry.clone());
            }
        }
    }
    require(
        journal.len() <= 124,
        "execution registration history requires checkpoint",
    )?;
    let pins: Vec<_> = combined.into_values().collect();
    let state = replay_policies(&pins, &journal)?;
    for pin in &owner_pins {
        require(
            pin.minimum_head
                .as_ref()
                .is_none_or(|head| state.roots.contains_key(head)),
            "execution registration rolled back personal history",
        )?;
    }
    Ok((
        pins,
        ClientAuthorizationBundle {
            journal,
            grant_leaf: bundle.grant_leaf,
        },
        state,
    ))
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen)]
pub struct PreparedExecutionServiceRegistration {
    request: ExecutionServiceRegistrationRequest,
    pins: Vec<PolicyPin>,
    bundle: ClientAuthorizationBundle,
    material: RecoveryMaterial,
    proof_counts: RegistrationProofCounts,
}

/// Public owner and source histories come from verified Node reads, never private checkpoints.
#[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen(js_name = prepareExecutionServiceRegistration))]
pub fn prepare_execution_service_registration(
    source_project_json: &str,
    owner_material_json: &str,
    signer: &Identity,
    pins_json: &str,
    bundle_json: &str,
    owner_pins_json: &str,
    owner_bundle_json: &str,
    target_node_public_key: &str,
    now_ms: f64,
    expires_at_ms: f64,
) -> crate::Result<PreparedExecutionServiceRegistration> {
    let _signature_scope = ScopedSignatureVerification::new();
    let prepare = || -> Result<_> {
        bootstrap::recovery::inspect_account_recovery_material(owner_material_json, now_ms)
            .map_err(|_| {
                AuthorizationError::InvalidProof("invalid execution owner's private material")
            })?;
        let material: RecoveryMaterial = parse(owner_material_json)?;
        let private_state = bootstrap::recovery::material_state(&material)?;
        let root = Identity::from_secret_hex(&material.root_secret_hex)
            .map_err(|_| AuthorizationError::InvalidProof("invalid execution owner root"))?;
        let source: ResourceRef = parse(source_project_json)?;
        let account = material.scope.account_id.clone();
        let personal = SpaceId::personal(&account);
        let now = bootstrap::timestamp(now_ms)?;
        let owner_bundle: ClientAuthorizationBundle = parse(owner_bundle_json)?;
        require(
            owner_bundle.grant_leaf == material.bundle.grant_leaf,
            "execution registration requires the current public owner entry authorization",
        )?;
        verify_client_authorization(
            owner_bundle_json,
            owner_pins_json,
            account.as_str(),
            personal.as_str(),
            &signer.public_key_hex(),
            now_ms,
        )
        .map_err(|_| {
            AuthorizationError::InvalidProof("invalid public execution owner authorization")
        })?;
        let (pins, bundle, state) = merged_context(
            &account,
            parse(owner_pins_json)?,
            owner_bundle,
            parse(pins_json)?,
            parse(bundle_json)?,
        )?;
        for pin in material.pins.iter().filter(|pin| {
            pin.domain == PolicyDomain::Account(account.clone())
                || pin.domain == PolicyDomain::Space(personal.clone())
        }) {
            require(
                pins.iter().any(|public| {
                    public.domain == pin.domain && public.genesis_root == pin.genesis_root
                }) && pin
                    .minimum_head
                    .as_ref()
                    .is_none_or(|head| state.roots.contains_key(head)),
                "public execution owner history conflicts with private recovery trust",
            )?;
        }
        require(
            state.account(&account).is_some_and(|current| {
                current.current_root == PublicKey::of(&root)
                    && private_state
                        .account(&account)
                        .is_some_and(|saved| saved.recovery == current.recovery)
            }) && state.space(&personal).is_some_and(|current| {
                private_state.space(&personal).is_some_and(|saved| {
                    current.current_root == saved.current_root && current.recovery == saved.recovery
                })
            }),
            "execution owner's material is not its current root",
        )?;
        let expiry = bootstrap::timestamp(expires_at_ms)?;
        validate_window(now, expiry, now)?;
        let target = PublicKey::parse(target_node_public_key)?;
        let verified = authorization(
            &state,
            &account,
            &PublicKey::of(signer),
            &source,
            &bundle.grant_leaf,
            &target,
            now,
            &mut Budget::default(),
        )?;
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
                source_project: source,
                source_grant: bundle.grant_leaf.clone(),
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
        let proof_counts =
            verify_execution_service_registration(&state, &request, &target, now)?.proof_counts();
        Ok(PreparedExecutionServiceRegistration {
            request,
            pins,
            bundle,
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

fn projection_pins(pins: &[PolicyPin], bundle: &ClientAuthorizationBundle) -> Vec<PolicyPin> {
    let domains: BTreeSet<_> = bundle
        .journal
        .iter()
        .filter_map(|entry| match entry {
            JournalEntry::AccountGenesis(value) | JournalEntry::Account(value) => {
                Some(PolicyDomain::Account(value.payload.account.clone()))
            }
            JournalEntry::AccountCommit(value) => Some(PolicyDomain::Account(
                value.proposal.payload.account.clone(),
            )),
            JournalEntry::SpaceGenesis(value) | JournalEntry::Space(value) => {
                Some(PolicyDomain::Space(value.payload.space.clone()))
            }
            JournalEntry::SpaceCommit(value) => {
                Some(PolicyDomain::Space(value.proposal.payload.space.clone()))
            }
            _ => None,
        })
        .collect();
    pins.iter()
        .filter(|pin| domains.contains(&pin.domain))
        .cloned()
        .collect()
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen)]
pub struct VerifiedExecutionServiceRegistration {
    resource: ResourceRef,
    pins: Vec<PolicyPin>,
    bundle: ClientAuthorizationBundle,
    scope: ClientScope,
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
        let mut state = replay_policies(&prepared.pins, &prepared.bundle.journal)?;
        let mut verified = verify_execution_service_registration_commit(
            &state,
            &prepared.request,
            &committed,
            None,
            &prepared.request.intent.payload.target_node_public_key,
            accepted,
        )?;
        require(
            committed.proposal == prepared.request.request.proposal
                && committed.commit.payload.nonce == prepared.request.request.nonce
                && committed.commit.payload.dependencies == verified.commit_heads,
            "execution registration receipt belongs to another root approval",
        )?;
        let [
            JournalEntry::Resource(resource),
            JournalEntry::Grant(parent),
        ] = prepared.request.request.new_objects.as_slice()
        else {
            return Err(AuthorizationError::InvalidProof(
                "missing prepared execution objects",
            ));
        };
        state.install_resource(resource.clone())?;
        state.install_grant(parent.clone(), accepted)?;
        let head = state.replay_space_committed(committed.clone())?;
        let mut pins = prepared.pins.clone();
        pins.iter_mut()
            .find(|pin| pin.domain == head.domain)
            .ok_or(AuthorizationError::PolicyConflict)?
            .minimum_head = Some(head.clone());
        let mut journal = prepared.bundle.journal.clone();
        journal.extend(prepared.request.request.new_objects.clone());
        journal.push(JournalEntry::SpaceCommit(committed));
        let intent = &prepared.request.intent;
        let personal = &intent.payload.execution_resource.space;
        let bundle = state.client_authorization_projection(
            &pins,
            &journal,
            &intent.payload.account,
            personal,
            &intent.issuer,
            &parent.hash()?,
            accepted,
        )?;
        let personal_pins = projection_pins(&pins, &bundle);
        let owner_bundle = state.client_authorization_projection(
            &pins,
            &journal,
            &intent.payload.account,
            personal,
            &intent.issuer,
            &prepared.material.bundle.grant_leaf,
            accepted,
        )?;
        let owner_pins = projection_pins(&pins, &owner_bundle);
        let scope = ClientScope {
            account_id: intent.payload.account.clone(),
            space_id: personal.clone(),
            device_id: DeviceId::from_key(&intent.issuer),
        };
        let mut material = RecoveryMaterial {
            format: prepared.material.format.clone(),
            version: prepared.material.version,
            root_secret_hex: prepared.material.root_secret_hex.clone(),
            recovery_secret_hex: prepared.material.recovery_secret_hex.clone(),
            scope: scope.clone(),
            pins: owner_pins,
            bundle: owner_bundle,
            entry: prepared.material.entry.clone(),
            policy_checkpoint: None,
        };
        bootstrap::recovery::compact_recovery_material(&mut material, now)?;
        let material_json =
            zeroize::Zeroizing::new(json(&material).map_err(|_| {
                AuthorizationError::InvalidProof("private material encoding failed")
            })?);
        bootstrap::recovery::inspect_account_recovery_material(&material_json, now_ms).map_err(
            |_| AuthorizationError::InvalidProof("updated execution owner material is invalid"),
        )?;
        verified.crypto_counts = SignatureVerification::current_counts();
        Ok(VerifiedExecutionServiceRegistration {
            resource: intent.payload.execution_resource.clone(),
            pins: personal_pins,
            bundle,
            scope,
            material_json,
            proof_counts: verified.proof_counts(),
        })
    };
    finish().map_err(|error| error.to_string())
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen)]
impl VerifiedExecutionServiceRegistration {
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
    #[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen(js_name = bundleJson))]
    pub fn bundle_json(&self) -> crate::Result<String> {
        json(&self.bundle)
    }
    #[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen(js_name = pinsJson))]
    pub fn pins_json(&self) -> crate::Result<String> {
        json(&self.pins)
    }
    #[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen(js_name = scopeJson))]
    pub fn scope_json(&self) -> crate::Result<String> {
        json(&self.scope)
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
    fn registration() -> (VerifiedPolicySet, PreparedExecutionServiceRegistration) {
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
        let bundle: ClientAuthorizationBundle = parse(&created.bundle_json().unwrap()).unwrap();
        let state = replay_policies(&pins, &bundle.journal).unwrap();
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
        let registration = prepare_execution_service_registration(
            &json(&source).unwrap(),
            &material,
            &key(3),
            &json(&pins).unwrap(),
            &json(&bundle).unwrap(),
            &json(&pins).unwrap(),
            &json(&bundle).unwrap(),
            &key(4).public_key_hex(),
            NOW,
            NOW + 60_000.0,
        )
        .unwrap();
        (state, registration)
    }
    fn committed(
        state: &VerifiedPolicySet,
        prepared: &PreparedExecutionServiceRegistration,
    ) -> Committed<SpaceDocument> {
        let verified = verify_execution_service_registration(
            state,
            &prepared.request,
            &PublicKey::of(&key(4)),
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
        let receipts = challenge
            .payload
            .dependencies
            .iter()
            .map(|head| {
                Signed::sign(
                    FreshnessReceipt {
                        version: AUTH_VERSION,
                        current: head.clone(),
                        challenge_hash: challenge.challenge_hash().unwrap(),
                        issued_at_ms: NOW as u64,
                        expires_at_ms: challenge.payload.expires_at_ms,
                    },
                    &key(4),
                )
                .unwrap()
            })
            .collect();
        let mut accepted = challenge.payload;
        accepted.accepted_at_ms = NOW as u64;
        Committed {
            proposal: prepared.request.request.proposal.clone(),
            commit: Signed::sign(accepted, &key(4)).unwrap(),
            receipts,
        }
    }

    #[test]
    fn registration_requires_independent_public_owner_history() {
        let (_, prepared) = registration();
        let mut incomplete = prepared.bundle.clone();
        incomplete
            .journal
            .retain(|entry| !matches!(entry, JournalEntry::AccountGenesis(_)));
        let result = prepare_execution_service_registration(
            &json(&prepared.request.intent.payload.source_project).unwrap(),
            &json(&prepared.material).unwrap(),
            &key(3),
            &json(&prepared.pins).unwrap(),
            &json(&prepared.bundle).unwrap(),
            &json(&prepared.pins).unwrap(),
            &json(&incomplete).unwrap(),
            &key(4).public_key_hex(),
            NOW,
            NOW + 60_000.0,
        );
        assert!(
            matches!(result, Err(error) if error.contains("invalid public execution owner authorization"))
        );
    }

    #[test]
    fn registration_commit_counts_root_source_and_receipt_objects_in_one_budget() {
        let (state, prepared) = registration();
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
        let (state, prepared) = registration();
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
