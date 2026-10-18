//! Team creation and capability-authorized space policy changes.

use super::*;
use super::{
    client_policy_import::require_confirmed_client_policy, policy_import::VerifiedPolicySnapshot,
};
use crate::signature_verification::{ScopedSignatureVerification, SignatureVerification};
use zeroize::Zeroize;

fn parse<T: for<'de> Deserialize<'de>>(value: &str) -> Result<T> {
    require(value.len() <= MAX_PROOF_BYTES, "governance JSON too large")?;
    serde_json::from_str(value)
        .map_err(|_| AuthorizationError::InvalidProof("invalid governance JSON"))
}
fn json<T: Serialize>(value: &T) -> crate::Result<String> {
    serde_json::to_string(value).map_err(|_| "governance encoding failed".to_owned())
}
fn time(value: f64) -> Result<u64> {
    bootstrap::timestamp(value)
}
fn load(
    authorization: &VerifiedClientAuthorization,
    signer: &PublicKey,
    now: u64,
) -> Result<(VerifiedPolicySnapshot, VerifiedClientAuthorization)> {
    let snapshot = require_confirmed_client_policy(authorization)?;
    require(
        authorization.scope.device_id == DeviceId::from_key(signer)
            && authorization.resources.len() == 1,
        "governance changed its authenticated client or selection",
    )?;
    let selected = ResourceRef {
        space: authorization.scope.space_id.clone(),
        resource: authorization.resources[0].payload.resource.clone(),
    };
    let current = selected_authorization(
        snapshot,
        &authorization.grant_leaf,
        &authorization.scope.account_id,
        signer,
        &selected,
        now,
    )?;
    require(
        current.heads == authorization.heads
            && current.audience == authorization.audience
            && current.audience_public_key == authorization.audience_public_key,
        "governance requires explicit policy synchronization",
    )?;
    Ok((snapshot.clone(), current))
}

fn selected_authorization(
    snapshot: &VerifiedPolicySnapshot,
    grant: &ObjectHash,
    account: &AccountId,
    signer: &PublicKey,
    selected: &ResourceRef,
    now: u64,
) -> Result<VerifiedClientAuthorization> {
    let mut current = verify_client_policy_state_selected(
        snapshot.state().clone(),
        &ClientAuthorizationBundle {
            journal: Vec::new(),
            grant_leaf: grant.clone(),
        },
        snapshot.pins(),
        account.clone(),
        selected.space.clone(),
        signer.clone(),
        Some(selected),
        now,
    )?;
    current.imported_snapshot = Some(snapshot.clone());
    Ok(current)
}

fn finish_usage(usage: &mut execution_handoff::ExecutionProofUsage) -> Result<()> {
    if let Some(counts) = SignatureVerification::current_counts() {
        usage.record_actual_signature_counts(counts);
    }
    usage.enforce_limits()
}

fn record_governance_usage(
    usage: &mut execution_handoff::ExecutionProofUsage,
    verified: &VerifiedSpaceGovernanceRequest,
) -> Result<()> {
    usage.record_budget(&verified.budget)?;
    usage.record_signed(&verified.request.proposal)?;
    for entry in &verified.request.new_objects {
        match entry {
            JournalEntry::Membership(value) => usage.record_signed(value)?,
            JournalEntry::Grant(value) => usage.record_signed(value)?,
            JournalEntry::Resource(value) => usage.record_signed(value)?,
            _ => {
                return Err(AuthorizationError::InvalidProof(
                    "unexpected governance delta object",
                ));
            }
        }
    }
    usage.record_metadata(&verified.request)
}

fn next_space(
    state: &VerifiedPolicySet,
    space: &SpaceId,
    grant: &ObjectHash,
) -> Result<SpaceDocument> {
    let mut document = state
        .space(space)
        .ok_or(AuthorizationError::PolicyConflict)?
        .clone();
    document.previous = state.head(&PolicyDomain::Space(space.clone())).cloned();
    document.sequence = document
        .sequence
        .checked_add(1)
        .ok_or(AuthorizationError::PolicyConflict)?;
    document.governance_grant = Some(grant.clone());
    document.root_acceptance = None;
    document.recovery_approvals.clear();
    Ok(document)
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SpaceMembershipIntent {
    pub space: SpaceId,
    pub subject_account: AccountId,
    pub roles: Vec<SpaceRole>,
    pub expires_at_ms: u64,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct DirectionalGrantIntent {
    pub space: SpaceId,
    pub grantee: Grantee,
    pub scope: ResourceScope,
    pub actions: Vec<Action>,
    pub audience_node_public_key: PublicKey,
    pub constraints: GrantConstraints,
    pub remaining_delegations: u8,
    pub expires_at_ms: u64,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SpaceResourceIntent {
    pub space: SpaceId,
    pub resource: ResourceId,
    pub kind: ResourceKind,
    pub host_node_public_key: PublicKey,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SpaceGovernanceRequest {
    pub proposal: Signed<SpaceDocument>,
    pub new_objects: Vec<JournalEntry>,
    pub nonce: Nonce,
}

#[derive(Clone, Debug)]
pub struct VerifiedSpaceGovernanceRequest {
    request: SpaceGovernanceRequest,
    dependencies: Vec<PolicyStamp>,
    expires: u64,
    budget: Budget,
}
impl VerifiedSpaceGovernanceRequest {
    pub fn request(&self) -> &SpaceGovernanceRequest {
        &self.request
    }
    pub fn expected_heads(&self) -> &[PolicyStamp] {
        &self.dependencies
    }
    pub fn expires_at_ms(&self) -> u64 {
        self.expires
    }
}

fn install_objects(
    state: &mut VerifiedPolicySet,
    request: &SpaceGovernanceRequest,
    now: u64,
) -> Result<()> {
    require(
        request.new_objects.len() <= 1,
        "governance change has too many new objects",
    )?;
    let space = &request.proposal.payload.space;
    let previous = state
        .space(space)
        .ok_or(AuthorizationError::PolicyConflict)?
        .clone();
    for entry in &request.new_objects {
        match entry {
            JournalEntry::Membership(value) => {
                let hash = value.hash()?;
                require(
                    value.issuer == request.proposal.issuer
                        && value.payload.space == *space
                        && !previous.memberships.contains(&hash)
                        && request.proposal.payload.memberships.contains(&hash),
                    "membership does not belong to proposed change",
                )?;
                state.install_membership(value.clone(), now)?;
            }
            JournalEntry::Grant(value) => {
                let hash = value.hash()?;
                require(
                    value.issuer == request.proposal.issuer
                        && value.payload.owner_space == *space
                        && !previous.grants.contains(&hash)
                        && request.proposal.payload.grants.contains(&hash),
                    "grant does not belong to proposed change",
                )?;
                state.install_grant(value.clone(), now)?;
            }
            JournalEntry::Resource(value) => {
                let hash = value.hash()?;
                require(
                    value.issuer == request.proposal.issuer
                        && value.payload.space == *space
                        && !previous.resources.contains(&hash)
                        && request.proposal.payload.resources.contains(&hash),
                    "resource does not belong to proposed change",
                )?;
                let host = previous
                    .devices
                    .iter()
                    .filter_map(|hash| state.device(hash))
                    .find(|certificate| {
                        certificate.payload.subject == Actor::Service(value.payload.host.clone())
                    })
                    .ok_or(AuthorizationError::InvalidProof(
                        "resource host is not an explicit space node reference",
                    ))?;
                state.verify_service_device(space, &host.payload.device, now)?;
                state.install_resource(value.clone())?;
            }
            _ => {
                return Err(AuthorizationError::InvalidProof(
                    "unsupported space governance object",
                ));
            }
        }
    }
    Ok(())
}

pub fn verify_space_governance_request(
    state: &VerifiedPolicySet,
    request: &SpaceGovernanceRequest,
    now: u64,
) -> Result<VerifiedSpaceGovernanceRequest> {
    require(
        canonical(request)?.len() <= MAX_PROOF_BYTES,
        "governance request too large",
    )?;
    let mut state = state.clone();
    let previous = state
        .space(&request.proposal.payload.space)
        .ok_or(AuthorizationError::PolicyConflict)?
        .clone();
    advance(
        &request.proposal.payload.previous,
        request.proposal.payload.authority_epoch,
        request.proposal.payload.sequence,
        state
            .head(&PolicyDomain::Space(previous.space.clone()))
            .ok_or(AuthorizationError::PolicyConflict)?,
    )?;
    install_objects(&mut state, request, now)?;
    state.validate_space(&request.proposal.payload)?;
    require(
        previous
            .revoked_devices
            .iter()
            .all(|device| request.proposal.payload.revoked_devices.contains(device)),
        "space governance cannot remove prior revocations",
    )?;
    let mut budget = Budget::default();
    budget.verify(&request.proposal)?;
    state.authorize_space_delta(&request.proposal, &previous, now, &mut budget)?;
    let mut dependencies = budget.dependencies.clone();
    state.dependency(&PolicyDomain::Space(previous.space), &mut dependencies)?;
    state.expand_authorities(now, &mut budget, &mut dependencies)?;
    Ok(VerifiedSpaceGovernanceRequest {
        request: request.clone(),
        dependencies: dependencies.into_values().collect(),
        expires: budget.expires_at_ms,
        budget,
    })
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen)]
pub struct PreparedSpaceGovernanceChange {
    request: SpaceGovernanceRequest,
    snapshot: VerifiedPolicySnapshot,
    authorization: VerifiedClientAuthorization,
    usage: execution_handoff::ExecutionProofUsage,
}
fn prepared(
    snapshot: VerifiedPolicySnapshot,
    authorization: VerifiedClientAuthorization,
    document: SpaceDocument,
    new_objects: Vec<JournalEntry>,
    signer: &Identity,
    now: u64,
) -> Result<PreparedSpaceGovernanceChange> {
    let request = SpaceGovernanceRequest {
        proposal: Signed::sign(document, signer)?,
        new_objects,
        nonce: Nonce::random()?,
    };
    let verified = verify_space_governance_request(snapshot.state(), &request, now)?;
    let mut usage = execution_handoff::ExecutionProofUsage::default();
    usage.record_budget(&authorization.budget)?;
    record_governance_usage(&mut usage, &verified)?;
    usage.record_chain_depth(
        snapshot
            .state()
            .grant_chain(Some(&authorization.grant_leaf))?
            .len(),
    );
    finish_usage(&mut usage)?;
    Ok(PreparedSpaceGovernanceChange {
        request,
        snapshot,
        authorization,
        usage,
    })
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen(js_name = prepareSpaceMembershipChange))]
pub fn prepare_space_membership_change(
    intent_json: &str,
    signer: &Identity,
    authorization: &VerifiedClientAuthorization,
    now_ms: f64,
) -> crate::Result<PreparedSpaceGovernanceChange> {
    let _signature_scope = ScopedSignatureVerification::new();
    let build = || -> Result<_> {
        let intent: SpaceMembershipIntent = parse(intent_json)?;
        require(
            intent.roles.len() <= 4 && sorted_unique(&intent.roles),
            "invalid membership roles",
        )?;
        let now = time(now_ms)?;
        let (snapshot, current) = load(authorization, &PublicKey::of(signer), now)?;
        let state = snapshot.state();
        let mut document = next_space(&state, &intent.space, &current.grant_leaf)?;
        document.memberships.retain(|hash| {
            state.membership(hash).is_none_or(|value| {
                value.payload.subject != Actor::Account(intent.subject_account.clone())
            })
        });
        let mut objects = Vec::new();
        if !intent.roles.is_empty() {
            validate_window(now, intent.expires_at_ms, now)?;
            let membership = Signed::sign(
                Membership {
                    version: AUTH_VERSION,
                    space: intent.space.clone(),
                    subject: Actor::Account(intent.subject_account),
                    roles: intent.roles,
                    issued_under: state
                        .head(&PolicyDomain::Space(intent.space))
                        .cloned()
                        .ok_or(AuthorizationError::PolicyConflict)?,
                    not_before_ms: now,
                    expires_at_ms: intent.expires_at_ms,
                    nonce: Nonce::random()?,
                },
                signer,
            )?;
            document.memberships.push(membership.hash()?);
            objects.push(JournalEntry::Membership(membership));
        }
        document.memberships.sort();
        prepared(snapshot, current, document, objects, signer, now)
    };
    build().map_err(|error| error.to_string())
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen(js_name = prepareDirectionalGrant))]
pub fn prepare_directional_grant(
    intent_json: &str,
    signer: &Identity,
    authorization: &VerifiedClientAuthorization,
    now_ms: f64,
) -> crate::Result<PreparedSpaceGovernanceChange> {
    let intent = parse(intent_json).map_err(|error| error.to_string())?;
    prepare_directional_grant_inner(intent, None, signer, authorization, now_ms)
}

pub(super) fn prepare_execution_source_grant(
    intent: DirectionalGrantIntent,
    source: super::remote_share::VerifiedExecutionSourcePolicy,
    signer: &Identity,
    authorization: &VerifiedClientAuthorization,
    now_ms: f64,
) -> crate::Result<PreparedSpaceGovernanceChange> {
    prepare_directional_grant_inner(intent, Some(source), signer, authorization, now_ms)
}

fn prepare_directional_grant_inner(
    intent: DirectionalGrantIntent,
    execution_source: Option<super::remote_share::VerifiedExecutionSourcePolicy>,
    signer: &Identity,
    authorization: &VerifiedClientAuthorization,
    now_ms: f64,
) -> crate::Result<PreparedSpaceGovernanceChange> {
    let _signature_scope = ScopedSignatureVerification::new();
    let build = || -> Result<_> {
        let now = time(now_ms)?;
        let (snapshot, current) = load(authorization, &PublicKey::of(signer), now)?;
        let state = snapshot.state();
        let target = state.verify_service_device(
            &intent.space,
            &DeviceId::from_key(&intent.audience_node_public_key),
            now,
        )?;
        require(
            target.payload.public_key == intent.audience_node_public_key,
            "grant audience key mismatch",
        )?;
        let mut document = next_space(&state, &intent.space, &current.grant_leaf)?;
        let grant = Signed::sign(
            Grant {
                version: AUTH_VERSION,
                owner_space: intent.space.clone(),
                scope: intent.scope,
                grantee: intent.grantee,
                actions: intent.actions,
                audience: Audience {
                    service: ServiceId::from_key(&intent.audience_node_public_key),
                    device: target.payload.device.clone(),
                },
                constraints: intent.constraints,
                execution_source: execution_source.map(|source| source.into_policy()),
                parent: Some(current.grant_leaf.clone()),
                remaining_delegations: intent.remaining_delegations,
                issued_under: state
                    .head(&PolicyDomain::Space(intent.space))
                    .cloned()
                    .ok_or(AuthorizationError::PolicyConflict)?,
                not_before_ms: now,
                expires_at_ms: intent.expires_at_ms,
                nonce: Nonce::random()?,
            },
            signer,
        )?;
        document.grants.push(grant.hash()?);
        document.grants.sort();
        prepared(
            snapshot,
            current,
            document,
            vec![JournalEntry::Grant(grant)],
            signer,
            now,
        )
    };
    build().map_err(|error| error.to_string())
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen(js_name = prepareSpaceGrantRevocation))]
pub fn prepare_space_grant_revocation(
    space_id: &str,
    grant_hash: &str,
    signer: &Identity,
    authorization: &VerifiedClientAuthorization,
    now_ms: f64,
) -> crate::Result<PreparedSpaceGovernanceChange> {
    let _signature_scope = ScopedSignatureVerification::new();
    let build = || -> Result<_> {
        let now = time(now_ms)?;
        let (snapshot, current) = load(authorization, &PublicKey::of(signer), now)?;
        let state = snapshot.state();
        let mut document = next_space(&state, &SpaceId::parse(space_id)?, &current.grant_leaf)?;
        let hash = ObjectHash::parse(grant_hash)?;
        require(
            document.grants.contains(&hash),
            "cannot revoke an unregistered grant",
        )?;
        document.grants.retain(|value| value != &hash);
        prepared(snapshot, current, document, Vec::new(), signer, now)
    };
    build().map_err(|error| error.to_string())
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen(js_name = prepareSpaceResourceRegistration))]
pub fn prepare_space_resource_registration(
    intent_json: &str,
    signer: &Identity,
    authorization: &VerifiedClientAuthorization,
    now_ms: f64,
) -> crate::Result<PreparedSpaceGovernanceChange> {
    let _signature_scope = ScopedSignatureVerification::new();
    let build = || -> Result<_> {
        let intent: SpaceResourceIntent = parse(intent_json)?;
        let now = time(now_ms)?;
        let (snapshot, current) = load(authorization, &PublicKey::of(signer), now)?;
        let state = snapshot.state();
        state.verify_service_device(
            &intent.space,
            &DeviceId::from_key(&intent.host_node_public_key),
            now,
        )?;
        require(
            state.resource(&intent.space, &intent.resource).is_none(),
            "resource identity already registered",
        )?;
        let mut document = next_space(&state, &intent.space, &current.grant_leaf)?;
        let record = Signed::sign(
            ResourceRecord {
                version: AUTH_VERSION,
                space: intent.space.clone(),
                resource: intent.resource,
                kind: intent.kind,
                host: ServiceId::from_key(&intent.host_node_public_key),
                issued_under: state
                    .head(&PolicyDomain::Space(intent.space))
                    .cloned()
                    .ok_or(AuthorizationError::PolicyConflict)?,
                nonce: Nonce::random()?,
            },
            signer,
        )?;
        document.resources.push(record.hash()?);
        document.resources.sort();
        prepared(
            snapshot,
            current,
            document,
            vec![JournalEntry::Resource(record)],
            signer,
            now,
        )
    };
    build().map_err(|error| error.to_string())
}

impl PreparedSpaceGovernanceChange {
    pub(crate) fn proof_usage(&self) -> &execution_handoff::ExecutionProofUsage {
        &self.usage
    }
}
#[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen)]
impl PreparedSpaceGovernanceChange {
    #[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen(js_name = requestJson))]
    pub fn request_json(&self) -> crate::Result<String> {
        json(&self.request)
    }
    #[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen(js_name = proofUsageJson))]
    pub fn proof_usage_json(&self) -> crate::Result<String> {
        json(&self.usage.counts())
    }
}

pub fn issue_space_governance_challenge_in_state(
    state: &VerifiedPolicySet,
    request: &SpaceGovernanceRequest,
    authority: &Identity,
    now: u64,
) -> Result<Signed<PolicyCommit>> {
    let _signature_scope = ScopedSignatureVerification::new();
    let verified = verify_space_governance_request(state, request, now)?;
    let space = state
        .space(&request.proposal.payload.space)
        .ok_or(AuthorizationError::PolicyConflict)?;
    require(
        space.authority.public_key == PublicKey::of(authority),
        "space challenge requires its current authority",
    )?;
    let challenge = Signed::sign(
        PolicyCommit {
            version: AUTH_VERSION,
            previous: state
                .head(&PolicyDomain::Space(space.space.clone()))
                .cloned()
                .ok_or(AuthorizationError::PolicyConflict)?,
            proposal_hash: request.proposal.hash()?,
            accepted_at_ms: 0,
            issued_at_ms: now,
            expires_at_ms: now.saturating_add(EXECUTION_LEASE_MS).min(verified.expires),
            nonce: request.nonce.clone(),
            dependencies: verified.dependencies.clone(),
        },
        authority,
    )?;
    let mut usage = execution_handoff::ExecutionProofUsage::default();
    record_governance_usage(&mut usage, &verified)?;
    usage.record_signed(&challenge)?;
    finish_usage(&mut usage)?;
    Ok(challenge)
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen(js_name = issueSpaceGovernanceChallenge))]
pub fn issue_space_governance_challenge(
    request_json: &str,
    authority: &Identity,
    authorization: &VerifiedClientAuthorization,
    now_ms: f64,
) -> crate::Result<String> {
    let _signature_scope = ScopedSignatureVerification::new();
    let issue = || -> Result<_> {
        let now = time(now_ms)?;
        let source = require_confirmed_client_policy(authorization)?;
        let signer = source
            .state()
            .find_device(&authorization.scope.device_id)?
            .payload
            .public_key
            .clone();
        let (snapshot, _) = load(authorization, &signer, now)?;
        let request: SpaceGovernanceRequest = parse(request_json)?;
        issue_space_governance_challenge_in_state(snapshot.state(), &request, authority, now)
    };
    json(&issue().map_err(|error| error.to_string())?)
}

pub fn accept_space_governance_change_in_state(
    state: &VerifiedPolicySet,
    request: &SpaceGovernanceRequest,
    challenge: &Signed<PolicyCommit>,
    receipts: &[Signed<FreshnessReceipt>],
    authority: &Identity,
    now: u64,
) -> Result<Committed<SpaceDocument>> {
    let _signature_scope = ScopedSignatureVerification::new();
    require(
        canonical(&(request, challenge, receipts))?.len() <= MAX_PROOF_BYTES,
        "governance commit input too large",
    )?;
    let verified = verify_space_governance_request(state, request, now)?;
    challenge.verify_signature()?;
    let space = state
        .space(&request.proposal.payload.space)
        .ok_or(AuthorizationError::PolicyConflict)?;
    require(
        challenge.issuer == PublicKey::of(authority)
            && challenge.issuer == space.authority.public_key
            && challenge.payload.version == AUTH_VERSION
            && challenge.payload.accepted_at_ms == 0
            && Some(&challenge.payload.previous)
                == state.head(&PolicyDomain::Space(space.space.clone()))
            && challenge.payload.proposal_hash == request.proposal.hash()?
            && challenge.payload.nonce == request.nonce
            && challenge.payload.dependencies == verified.dependencies,
        "space challenge differs from current governance request",
    )?;
    validate_window(
        challenge.payload.issued_at_ms,
        challenge.payload.expires_at_ms,
        now,
    )?;
    require(
        challenge.payload.expires_at_ms <= verified.expires,
        "space challenge exceeds signer capability",
    )?;
    let mut candidate = state.clone();
    install_objects(&mut candidate, request, now)?;
    let mut commit = challenge.payload.clone();
    commit.accepted_at_ms = now;
    let change = Committed {
        proposal: request.proposal.clone(),
        commit: Signed::sign(commit, authority)?,
        receipts: receipts.to_vec(),
    };
    candidate.install_space_committed(change.clone(), now)?;
    let mut usage = execution_handoff::ExecutionProofUsage::default();
    record_governance_usage(&mut usage, &verified)?;
    usage.record_signed(challenge)?;
    usage.record_signed(&change.commit)?;
    for receipt in receipts {
        usage.record_signed(receipt)?;
    }
    usage.record_metadata(&change)?;
    finish_usage(&mut usage)?;
    Ok(change)
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen(js_name = acceptSpaceGovernanceChange))]
pub fn accept_space_governance_change(
    request_json: &str,
    challenge_json: &str,
    receipts_json: &str,
    authority: &Identity,
    authorization: &VerifiedClientAuthorization,
    now_ms: f64,
) -> crate::Result<String> {
    let _signature_scope = ScopedSignatureVerification::new();
    let accept = || -> Result<_> {
        let now = time(now_ms)?;
        let source = require_confirmed_client_policy(authorization)?;
        let signer = source
            .state()
            .find_device(&authorization.scope.device_id)?
            .payload
            .public_key
            .clone();
        let (snapshot, _) = load(authorization, &signer, now)?;
        let request: SpaceGovernanceRequest = parse(request_json)?;
        let challenge: Signed<PolicyCommit> = parse(challenge_json)?;
        let receipts: Vec<Signed<FreshnessReceipt>> = parse(receipts_json)?;
        accept_space_governance_change_in_state(
            snapshot.state(),
            &request,
            &challenge,
            &receipts,
            authority,
            now,
        )
    };
    json(&accept().map_err(|error| error.to_string())?)
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen)]
pub struct VerifiedSpaceGovernanceChange {
    snapshot: VerifiedPolicySnapshot,
    authorization: Option<VerifiedClientAuthorization>,
    usage: execution_handoff::ExecutionProofUsage,
}
#[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen(js_name = finishSpaceGovernanceChange))]
pub fn finish_space_governance_change(
    prepared: &PreparedSpaceGovernanceChange,
    committed_json: &str,
    now_ms: f64,
) -> crate::Result<VerifiedSpaceGovernanceChange> {
    let _signature_scope = ScopedSignatureVerification::new();
    let finish = || -> Result<_> {
        let now = time(now_ms)?;
        let change: Committed<SpaceDocument> = parse(committed_json)?;
        let accepted = change.commit.payload.accepted_at_ms;
        require(accepted <= now, "space receipt is from the future")?;
        let verified = verify_space_governance_request(
            prepared.snapshot.state(),
            &prepared.request,
            accepted,
        )?;
        require(
            change.proposal == prepared.request.proposal
                && change.commit.payload.nonce == prepared.request.nonce,
            "space commit belongs to another prepared change",
        )?;
        let mut delta = prepared.request.new_objects.clone();
        delta.push(JournalEntry::SpaceCommit(change.clone()));
        let snapshot = prepared.snapshot.apply_delta(&delta)?;
        let mut pins = snapshot.pins().to_vec();
        for pin in &mut pins {
            pin.minimum_head = snapshot.state().head(&pin.domain).cloned();
        }
        let snapshot = snapshot.with_independent_pins(&pins)?;
        let selected = ResourceRef {
            space: prepared.authorization.scope.space_id.clone(),
            resource: prepared.authorization.resources[0].payload.resource.clone(),
        };
        let authorization = selected_authorization(
            &snapshot,
            &prepared.authorization.grant_leaf,
            &prepared.authorization.scope.account_id,
            &prepared.request.proposal.issuer,
            &selected,
            now,
        );
        let authorization = match authorization {
            Ok(current) => Some(current),
            Err(AuthorizationError::BudgetExceeded) => {
                return Err(AuthorizationError::BudgetExceeded);
            }
            Err(_) => None,
        };
        let mut usage = execution_handoff::ExecutionProofUsage::default();
        record_governance_usage(&mut usage, &verified)?;
        usage.record_signed(&change.commit)?;
        for receipt in &change.receipts {
            usage.record_signed(receipt)?;
        }
        if let Some(current) = &authorization {
            usage.record_budget(&current.budget)?;
        }
        usage.record_metadata(&change)?;
        usage.record_chain_depth(
            prepared
                .snapshot
                .state()
                .grant_chain(Some(&prepared.authorization.grant_leaf))?
                .len(),
        );
        finish_usage(&mut usage)?;
        Ok(VerifiedSpaceGovernanceChange {
            snapshot,
            authorization,
            usage,
        })
    };
    finish().map_err(|error| error.to_string())
}
impl VerifiedSpaceGovernanceChange {
    pub(crate) fn proof_usage(&self) -> &execution_handoff::ExecutionProofUsage {
        &self.usage
    }
    pub(crate) fn snapshot(&self) -> &VerifiedPolicySnapshot {
        &self.snapshot
    }
}
#[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen)]
impl VerifiedSpaceGovernanceChange {
    pub fn authorization(&self) -> Option<VerifiedClientAuthorization> {
        self.authorization.clone()
    }
    #[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen(js_name = pinsJson))]
    pub fn pins_json(&self) -> crate::Result<String> {
        json(&self.snapshot.pins())
    }
    #[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen(js_name = scopeJson))]
    pub fn scope_json(&self) -> crate::Result<String> {
        json(&self.authorization.as_ref().map(|value| value.scope()))
    }
    #[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen(js_name = proofUsageJson))]
    pub fn proof_usage_json(&self) -> crate::Result<String> {
        json(&self.usage.counts())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct TeamCreationRequest {
    pub version: u16,
    pub owner_account: AccountId,
    pub governor_device_public_key: PublicKey,
    pub target_node_public_key: PublicKey,
    pub source_project: ResourceRef,
    pub team_project: ResourceRef,
    pub expected_heads: Vec<PolicyStamp>,
    pub nonce: Nonce,
    pub issued_at_ms: u64,
    pub expires_at_ms: u64,
    pub team_pin: PolicyPin,
    pub journal_delta: Vec<JournalEntry>,
}
impl sealed::Sealed for TeamCreationRequest {}
impl SignedObject for TeamCreationRequest {
    const DOMAIN: &'static str = "team-creation-request-v2";
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct TeamCreationAcceptance {
    pub version: u16,
    pub request_hash: ObjectHash,
    pub space: SpaceId,
    pub owner_account: AccountId,
    pub target_node_public_key: PublicKey,
    pub account_head: PolicyStamp,
    pub nonce: Nonce,
    pub accepted_at_ms: u64,
    pub expires_at_ms: u64,
}
impl sealed::Sealed for TeamCreationAcceptance {}
impl SignedObject for TeamCreationAcceptance {
    const DOMAIN: &'static str = "team-creation-acceptance-v2";
}

#[derive(Clone, Debug)]
pub struct VerifiedTeamCreationRequest {
    signed: Signed<TeamCreationRequest>,
    request_hash: ObjectHash,
    account_head: PolicyStamp,
    grant: ObjectHash,
    budget: Budget,
}
impl VerifiedTeamCreationRequest {
    pub fn request(&self) -> &TeamCreationRequest {
        &self.signed.payload
    }
    pub fn request_hash(&self) -> &ObjectHash {
        &self.request_hash
    }
    pub fn expected_heads(&self) -> &[PolicyStamp] {
        &self.signed.payload.expected_heads
    }
    pub fn expires_at_ms(&self) -> u64 {
        self.signed.payload.expires_at_ms
    }
}

fn owner_actions() -> Vec<Action> {
    vec![
        Action::ConversationRead,
        Action::ConversationCreate,
        Action::MessageAppend,
        Action::TaskCreate,
        Action::TaskProgress,
        Action::TaskAccept,
        Action::TaskCancel,
        Action::ExecutionOpen,
        Action::ExecutionSend,
        Action::ExecutionCommand,
        Action::ExecutionSteer,
        Action::ExecutionInterrupt,
        Action::ExecutionRespondApproval,
        Action::ExecutionRebind,
        Action::OperationStatusRead,
        Action::OperationReconcile,
        Action::OperationResolve,
        Action::AttachmentRead,
        Action::AttachmentWrite,
        Action::Export,
        Action::NotificationRead,
        Action::NotificationAck,
        Action::MemberManage,
        Action::ResourceManage,
        Action::GrantIssue,
        Action::GrantRevoke,
        Action::ServiceEnroll,
        Action::ReplicaRead,
        Action::AgentStart,
        Action::AgentInterrupt,
    ]
}

fn source_authorization(
    state: &VerifiedPolicySet,
    source_grant: &ObjectHash,
    account: &AccountId,
    governor: &PublicKey,
    node: &PublicKey,
    source: &ResourceRef,
    now: u64,
) -> Result<(Vec<PolicyStamp>, u64, Budget)> {
    require(
        source.space == SpaceId::personal(account),
        "team source must be the owner's personal project",
    )?;
    let mut budget = Budget::default();
    let mut dependencies = BTreeMap::new();
    require(
        state.actor_for_signer(governor, now, &mut budget, &mut dependencies)?
            == Actor::Account(account.clone()),
        "team creator is not its current account client",
    )?;
    state.governance_capability(
        source_grant,
        governor,
        &source.space,
        Some(Action::ResourceManage),
        Some(&source.resource),
        now,
        &mut budget,
    )?;
    let target = state.verify_service_device(&source.space, &DeviceId::from_key(node), now)?;
    require(
        target.payload.public_key == *node
            && target.payload.owner == PolicyDomain::Account(account.clone()),
        "team creation cannot adopt another account's physical node",
    )?;
    let record = state
        .resource(&source.space, &source.resource)
        .ok_or(AuthorizationError::PolicyConflict)?;
    require(
        record.payload.kind == ResourceKind::Project
            && record.payload.host == ServiceId::from_key(node),
        "selected project is not hosted by the selected personal node",
    )?;
    state.validate_certificate(
        target,
        now,
        &mut budget,
        &mut dependencies,
        &mut BTreeSet::new(),
    )?;
    dependencies.extend(budget.dependencies.clone());
    state.dependency(
        &PolicyDomain::Space(source.space.clone()),
        &mut dependencies,
    )?;
    state.expand_authorities(now, &mut budget, &mut dependencies)?;
    Ok((
        dependencies.into_values().collect(),
        budget.expires_at_ms,
        budget,
    ))
}

pub fn verify_team_creation_request(
    state: &VerifiedPolicySet,
    source_grant: &ObjectHash,
    source_pins: &[PolicyPin],
    signed: &Signed<TeamCreationRequest>,
    now: u64,
) -> Result<VerifiedTeamCreationRequest> {
    require(
        canonical(signed)?.len() <= MAX_PROOF_BYTES,
        "team creation request too large",
    )?;
    signed.verify_signature()?;
    let request = &signed.payload;
    validate_window(request.issued_at_ms, request.expires_at_ms, now)?;
    require(
        request.version == AUTH_VERSION
            && signed.issuer == request.governor_device_public_key
            && request.expires_at_ms <= request.issued_at_ms.saturating_add(INVOCATION_MAX_AGE_MS)
            && request.team_project.resource != request.source_project.resource,
        "invalid team creation envelope",
    )?;
    let (heads, deadline, source_budget) = source_authorization(
        state,
        source_grant,
        &request.owner_account,
        &signed.issuer,
        &request.target_node_public_key,
        &request.source_project,
        now,
    )?;
    require(
        request.expected_heads == heads && request.expires_at_ms <= deadline,
        "team creation changed policy heads or exceeds creator capability",
    )?;
    let root = &request.team_pin.genesis_root;
    let space = SpaceId::team(root);
    require(
        request.team_project.space == space
            && request.team_pin.domain == PolicyDomain::Space(space.clone())
            && state.space(&space).is_none()
            && !state.roots.values().any(|key| key == root)
            && !state
                .devices
                .values()
                .any(|value| &value.payload.public_key == root)
            && !state
                .accounts
                .values()
                .flat_map(|value| &value.history)
                .any(|value| value.payload.recovery.keys.contains(root)),
        "team root must be a new independent key",
    )?;
    let [
        JournalEntry::SpaceGenesis(genesis),
        JournalEntry::Resource(resource),
        JournalEntry::Membership(membership),
        JournalEntry::Grant(grant),
        JournalEntry::Space(policy),
    ] = request.journal_delta.as_slice()
    else {
        return Err(AuthorizationError::InvalidProof(
            "invalid initial team journal",
        ));
    };
    require(
        [
            &genesis.issuer,
            &resource.issuer,
            &membership.issuer,
            &grant.issuer,
            &policy.issuer,
        ]
        .iter()
        .all(|issuer| *issuer == root),
        "initial team objects require the independent team root",
    )?;
    let owner = state
        .account(&request.owner_account)
        .ok_or(AuthorizationError::PolicyConflict)?;
    let genesis_head = space_stamp(genesis)?;
    require(
        genesis.payload.space == space
            && genesis.payload.root
                == SpaceRoot::Team {
                    genesis_root: root.clone(),
                }
            && genesis.payload.authority
                == AuthorityBinding::from_key(request.target_node_public_key.clone())
            && genesis.payload.recovery.generation == 0
            && genesis.payload.recovery.threshold == 1
            && genesis.payload.recovery.keys.len() == 1
            && owner
                .recovery
                .keys
                .contains(&genesis.payload.recovery.keys[0])
            && resource.payload.version == AUTH_VERSION
            && resource.payload.space == space
            && resource.payload.resource == request.team_project.resource
            && resource.payload.kind == ResourceKind::Project
            && resource.payload.host == ServiceId::from_key(&request.target_node_public_key)
            && resource.payload.issued_under == genesis_head
            && membership.payload.version == AUTH_VERSION
            && membership.payload.space == space
            && membership.payload.subject == Actor::Account(request.owner_account.clone())
            && membership.payload.roles == [SpaceRole::SpaceAdmin]
            && membership.payload.issued_under == genesis_head
            && membership.payload.not_before_ms == request.issued_at_ms,
        "initial team ownership, recovery or project differs from the request",
    )?;
    let target = state.find_device(&DeviceId::from_key(&request.target_node_public_key))?;
    require(
        membership.payload.expires_at_ms <= target.payload.expires_at_ms
            && grant.payload.version == AUTH_VERSION
            && grant.payload.owner_space == space
            && grant.payload.scope == ResourceScope::WholeSpace(space.clone())
            && grant.payload.grantee
                == Grantee::MembersOf(MembersOf {
                    space: space.clone(),
                    pinned_genesis: genesis.hash()?,
                    kind: MemberKind::Account,
                    roles_any: vec![SpaceRole::SpaceAdmin],
                })
            && grant.payload.actions == owner_actions()
            && grant.payload.audience
                == Audience {
                    service: ServiceId::from_key(&request.target_node_public_key),
                    device: target.payload.device.clone(),
                }
            && grant.payload.constraints == GrantConstraints::default()
            && grant.payload.parent.is_none()
            && grant.payload.remaining_delegations == MAX_DELEGATION_DEPTH as u8
            && grant.payload.issued_under == genesis_head
            && grant.payload.not_before_ms == request.issued_at_ms
            && grant.payload.expires_at_ms == membership.payload.expires_at_ms,
        "initial team grant exceeds its explicit administrator scope",
    )?;
    let mut expected = genesis.payload.clone();
    expected.previous = Some(genesis_head);
    expected.sequence = 1;
    expected.devices = vec![target.hash()?];
    expected.resources = vec![resource.hash()?];
    expected.memberships = vec![membership.hash()?];
    expected.grants = vec![grant.hash()?];
    require(
        policy.payload == expected
            && request.team_pin.minimum_head.as_ref() == Some(&space_stamp(policy)?),
        "initial team policy differs from its exact objects",
    )?;
    let mut pins: Vec<_> = source_pins
        .iter()
        .filter(|pin| pin.domain != request.team_pin.domain)
        .cloned()
        .collect();
    pins.push(request.team_pin.clone());
    pins.sort_by(|a, b| a.domain.cmp(&b.domain));
    let mut created = state.clone();
    for entry in &request.journal_delta {
        apply_policy_entry(&mut created, &pins, entry.clone())?;
    }
    require(
        created.space(&space).is_some(),
        "team creation did not produce its policy",
    )?;
    let account_head = state
        .head(&PolicyDomain::Account(request.owner_account.clone()))
        .cloned()
        .ok_or(AuthorizationError::PolicyConflict)?;
    Ok(VerifiedTeamCreationRequest {
        signed: signed.clone(),
        request_hash: ObjectHash::body(signed)?,
        account_head,
        grant: grant.hash()?,
        budget: source_budget,
    })
}

#[derive(Clone, Debug)]
pub(crate) struct VerifiedTeamCreationDelta {
    signed: Signed<TeamCreationRequest>,
    source_heads: Vec<PolicyStamp>,
}
impl VerifiedTeamCreationDelta {
    pub(crate) fn request(&self) -> &Signed<TeamCreationRequest> {
        &self.signed
    }
    pub(crate) fn source_heads(&self) -> &[PolicyStamp] {
        &self.source_heads
    }
}

pub(crate) struct VerifiedPendingTeamCreation {
    signed: Signed<TeamCreationRequest>,
    source_heads: Vec<PolicyStamp>,
}
impl VerifiedPendingTeamCreation {
    pub(crate) fn request(&self) -> &Signed<TeamCreationRequest> {
        &self.signed
    }
    pub(crate) fn source_heads(&self) -> &[PolicyStamp] {
        &self.source_heads
    }
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen)]
pub struct PreparedTeamCreation {
    request: Signed<TeamCreationRequest>,
    source: VerifiedPolicySnapshot,
    source_grant: ObjectHash,
    original_heads: Vec<PolicyStamp>,
    approved: VerifiedTeamCreationDelta,
    root_secret_hex: String,
    usage: execution_handoff::ExecutionProofUsage,
}
impl Drop for PreparedTeamCreation {
    fn drop(&mut self) {
        self.root_secret_hex.zeroize();
    }
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen(js_name = prepareTeamCreation))]
pub fn prepare_team_creation(
    team_root: &Identity,
    governor: &Identity,
    owner_account_id: &str,
    target_node_public_key: &str,
    recovery_public_key: &str,
    source_project_resource_id: &str,
    authorization: &VerifiedClientAuthorization,
    expires_at_ms: f64,
    now_ms: f64,
) -> crate::Result<PreparedTeamCreation> {
    let _signature_scope = ScopedSignatureVerification::new();
    let build = || -> Result<_> {
        let now = time(now_ms)?;
        let expires = time(expires_at_ms)?;
        validate_window(now, expires, now)?;
        let (snapshot, current) = load(authorization, &PublicKey::of(governor), now)?;
        let state = snapshot.state();
        let account = AccountId::parse(owner_account_id)?;
        let node = PublicKey::parse(target_node_public_key)?;
        let root = PublicKey::of(team_root);
        let space = SpaceId::team(&root);
        let source = ResourceRef {
            space: SpaceId::personal(&account),
            resource: ResourceId::parse(source_project_resource_id)?,
        };
        require(
            current.scope.account_id == account
                && current.scope.space_id == source.space
                && current.resources[0].payload.resource == source.resource,
            "team creation changed its authenticated personal project",
        )?;
        let (heads, deadline, _) = source_authorization(
            state,
            &current.grant_leaf,
            &account,
            &PublicKey::of(governor),
            &node,
            &source,
            now,
        )?;
        let recovery = PublicKey::parse(recovery_public_key)?;
        require(
            state
                .account(&account)
                .is_some_and(|document| document.recovery.keys.contains(&recovery)),
            "team recovery key is not an explicit current personal recovery key",
        )?;
        let genesis = Signed::sign(
            SpaceDocument {
                version: AUTH_VERSION,
                space: space.clone(),
                root: SpaceRoot::Team {
                    genesis_root: root.clone(),
                },
                current_root: root.clone(),
                root_generation: 0,
                previous: None,
                authority_epoch: 0,
                sequence: 0,
                authority: AuthorityBinding::from_key(node.clone()),
                recovery: RecoveryPolicy {
                    generation: 0,
                    keys: vec![recovery],
                    threshold: 1,
                },
                devices: Vec::new(),
                revoked_devices: Vec::new(),
                memberships: Vec::new(),
                grants: Vec::new(),
                resources: Vec::new(),
                governance_grant: None,
                root_acceptance: None,
                recovery_approvals: Vec::new(),
            },
            team_root,
        )?;
        let head = space_stamp(&genesis)?;
        let resource_id = ResourceId::random()?;
        let resource = Signed::sign(
            ResourceRecord {
                version: AUTH_VERSION,
                space: space.clone(),
                resource: resource_id.clone(),
                kind: ResourceKind::Project,
                host: ServiceId::from_key(&node),
                issued_under: head.clone(),
                nonce: Nonce::random()?,
            },
            team_root,
        )?;
        let membership = Signed::sign(
            Membership {
                version: AUTH_VERSION,
                space: space.clone(),
                subject: Actor::Account(account.clone()),
                roles: vec![SpaceRole::SpaceAdmin],
                issued_under: head.clone(),
                not_before_ms: now,
                expires_at_ms: expires,
                nonce: Nonce::random()?,
            },
            team_root,
        )?;
        let target = state.find_device(&DeviceId::from_key(&node))?;
        let grant = Signed::sign(
            Grant {
                execution_source: None,
                version: AUTH_VERSION,
                owner_space: space.clone(),
                scope: ResourceScope::WholeSpace(space.clone()),
                grantee: Grantee::MembersOf(MembersOf {
                    space: space.clone(),
                    pinned_genesis: genesis.hash()?,
                    kind: MemberKind::Account,
                    roles_any: vec![SpaceRole::SpaceAdmin],
                }),
                actions: owner_actions(),
                audience: Audience {
                    service: ServiceId::from_key(&node),
                    device: target.payload.device.clone(),
                },
                constraints: GrantConstraints::default(),
                parent: None,
                remaining_delegations: MAX_DELEGATION_DEPTH as u8,
                issued_under: head.clone(),
                not_before_ms: now,
                expires_at_ms: expires,
                nonce: Nonce::random()?,
            },
            team_root,
        )?;
        let mut document = genesis.payload.clone();
        document.previous = Some(head);
        document.sequence = 1;
        document.devices = vec![target.hash()?];
        document.resources = vec![resource.hash()?];
        document.memberships = vec![membership.hash()?];
        document.grants = vec![grant.hash()?];
        let policy = Signed::sign(document, team_root)?;
        let team_pin = PolicyPin {
            domain: PolicyDomain::Space(space.clone()),
            genesis_root: root,
            minimum_head: Some(space_stamp(&policy)?),
        };
        let request = Signed::sign(
            TeamCreationRequest {
                version: AUTH_VERSION,
                owner_account: account,
                governor_device_public_key: PublicKey::of(governor),
                target_node_public_key: node,
                source_project: source,
                team_project: ResourceRef {
                    space,
                    resource: resource_id,
                },
                expected_heads: heads,
                nonce: Nonce::random()?,
                issued_at_ms: now,
                expires_at_ms: now.saturating_add(INVOCATION_MAX_AGE_MS).min(deadline),
                team_pin,
                journal_delta: vec![
                    JournalEntry::SpaceGenesis(genesis),
                    JournalEntry::Resource(resource),
                    JournalEntry::Membership(membership),
                    JournalEntry::Grant(grant),
                    JournalEntry::Space(policy),
                ],
            },
            governor,
        )?;
        let verified = verify_team_creation_request(
            state,
            &current.grant_leaf,
            snapshot.pins(),
            &request,
            now,
        )?;
        let approved = VerifiedTeamCreationDelta {
            signed: request.clone(),
            source_heads: snapshot.heads().to_vec(),
        };
        let created = snapshot.with_created_team(&approved)?;
        let mut usage = execution_handoff::ExecutionProofUsage::default();
        usage.record_budget(&current.budget)?;
        record_team_usage(&mut usage, &request, &verified)?;
        usage.record_chain_depth(state.grant_chain(Some(&current.grant_leaf))?.len());
        finish_usage(&mut usage)?;
        require(
            created
                .state()
                .space(&request.payload.team_project.space)
                .is_some(),
            "new team snapshot is missing its exact policy",
        )?;
        Ok(PreparedTeamCreation {
            original_heads: snapshot.heads().to_vec(),
            request,
            source: snapshot,
            source_grant: current.grant_leaf,
            approved,
            root_secret_hex: team_root.secret_hex(),
            usage,
        })
    };
    build().map_err(|error| error.to_string())
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen)]
impl PreparedTeamCreation {
    #[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen(js_name = recoveryMaterialJson))]
    pub fn recovery_material_json(&self) -> crate::Result<String> {
        let request = &self.request.payload;
        let Some(JournalEntry::SpaceGenesis(genesis)) = request.journal_delta.first() else {
            return Err("missing prepared team genesis".to_owned());
        };
        let material = json(&TeamRecoveryMaterial {
            status: "pending",
            pending_request: &self.request,
            source_pins: self.source.pins(),
            source_heads: &self.original_heads,
            source_grant: &self.source_grant,
            format: "vibeface-team-recovery",
            version: AUTH_VERSION,
            root_secret_hex: &self.root_secret_hex,
            owner_account: &request.owner_account,
            team: &request.team_project.space,
            recovery_public_key: &genesis.payload.recovery.keys[0],
        })?;
        if material.len() > MAX_PROOF_BYTES {
            return Err("team private material exceeds its carrier".to_owned());
        }
        Ok(material)
    }
    #[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen(js_name = requestJson))]
    pub fn request_json(&self) -> crate::Result<String> {
        json(&self.request)
    }
    #[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen(js_name = teamId))]
    pub fn team_id(&self) -> String {
        self.request.payload.team_project.space.to_string()
    }
    #[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen(js_name = proofUsageJson))]
    pub fn proof_usage_json(&self) -> crate::Result<String> {
        json(&self.usage.counts())
    }
}

pub fn accept_team_creation_in_state(
    state: &VerifiedPolicySet,
    source_grant: &ObjectHash,
    pins: &[PolicyPin],
    request: &Signed<TeamCreationRequest>,
    node: &Identity,
    now: u64,
) -> Result<Signed<TeamCreationAcceptance>> {
    let _signature_scope = ScopedSignatureVerification::new();
    let verified = verify_team_creation_request(state, source_grant, pins, request, now)?;
    require(
        verified.request().target_node_public_key == PublicKey::of(node),
        "team creation reached another physical node",
    )?;
    let acceptance = Signed::sign(
        TeamCreationAcceptance {
            version: AUTH_VERSION,
            request_hash: verified.request_hash.clone(),
            space: request.payload.team_project.space.clone(),
            owner_account: request.payload.owner_account.clone(),
            target_node_public_key: PublicKey::of(node),
            account_head: verified.account_head.clone(),
            nonce: request.payload.nonce.clone(),
            accepted_at_ms: now,
            expires_at_ms: request.payload.expires_at_ms,
        },
        node,
    )?;
    let mut usage = execution_handoff::ExecutionProofUsage::default();
    record_team_usage(&mut usage, request, &verified)?;
    usage.record_signed(&acceptance)?;
    finish_usage(&mut usage)?;
    Ok(acceptance)
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen(js_name = acceptTeamCreation))]
pub fn accept_team_creation(
    request_json: &str,
    node: &Identity,
    authorization: &VerifiedClientAuthorization,
    now_ms: f64,
) -> crate::Result<String> {
    let _signature_scope = ScopedSignatureVerification::new();
    let accept = || -> Result<_> {
        let now = time(now_ms)?;
        let source = require_confirmed_client_policy(authorization)?;
        let signer = source
            .state()
            .find_device(&authorization.scope.device_id)?
            .payload
            .public_key
            .clone();
        let (snapshot, current) = load(authorization, &signer, now)?;
        let request: Signed<TeamCreationRequest> = parse(request_json)?;
        accept_team_creation_in_state(
            snapshot.state(),
            &current.grant_leaf,
            snapshot.pins(),
            &request,
            node,
            now,
        )
    };
    json(&accept().map_err(|error| error.to_string())?)
}

#[derive(Serialize)]
struct TeamRecoveryMaterial<'a> {
    status: &'static str,
    pending_request: &'a Signed<TeamCreationRequest>,
    source_pins: &'a [PolicyPin],
    source_heads: &'a [PolicyStamp],
    source_grant: &'a ObjectHash,
    format: &'static str,
    version: u16,
    root_secret_hex: &'a str,
    owner_account: &'a AccountId,
    team: &'a SpaceId,
    recovery_public_key: &'a PublicKey,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PendingTeamRecoveryMaterial {
    status: String,
    pending_request: Signed<TeamCreationRequest>,
    source_pins: Vec<PolicyPin>,
    source_heads: Vec<PolicyStamp>,
    source_grant: ObjectHash,
    format: String,
    version: u16,
    root_secret_hex: String,
    owner_account: AccountId,
    team: SpaceId,
    recovery_public_key: PublicKey,
}
impl Drop for PendingTeamRecoveryMaterial {
    fn drop(&mut self) {
        self.root_secret_hex.zeroize();
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct EarlierPendingTeamRecoveryMaterial {
    status: String,
    pending_request: Signed<TeamCreationRequest>,
    source_pins: Vec<PolicyPin>,
    source_bundle: ClientAuthorizationBundle,
    format: String,
    version: u16,
    root_secret_hex: String,
    owner_account: AccountId,
    team: SpaceId,
    recovery_public_key: PublicKey,
    pins: Vec<PolicyPin>,
    bundle: ClientAuthorizationBundle,
}
impl Drop for EarlierPendingTeamRecoveryMaterial {
    fn drop(&mut self) {
        self.root_secret_hex.zeroize();
    }
}

fn pending_material(
    value: &str,
    current: &VerifiedPolicySnapshot,
) -> Result<PendingTeamRecoveryMaterial> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Material {
        Compact(PendingTeamRecoveryMaterial),
        Earlier(EarlierPendingTeamRecoveryMaterial),
    }
    match parse(value)? {
        Material::Compact(material) => Ok(material),
        Material::Earlier(material) => {
            require(
                material.version == AUTH_VERSION,
                "unsupported team backup version",
            )?;
            for entry in &material.source_bundle.journal {
                let hash = ObjectHash::body(entry)?;
                let expected = current
                    .journal()
                    .iter()
                    .find(|candidate| ObjectHash::body(candidate).is_ok_and(|value| value == hash))
                    .ok_or_else(|| {
                        AuthorizationError::NeedPolicySync(
                            material.pending_request.payload.expected_heads.clone(),
                        )
                    })?;
                require(
                    canonical(expected)? == canonical(entry)?,
                    "pending team source commitment changed",
                )?;
            }
            let request = &material.pending_request.payload;
            let mut journal = material.source_bundle.journal.clone();
            journal.extend(request.journal_delta.clone());
            let Some(JournalEntry::Grant(grant)) = request.journal_delta.get(3) else {
                return Err(AuthorizationError::InvalidProof(
                    "missing pending team grant",
                ));
            };
            let mut pins = material.source_pins.clone();
            pins.push(request.team_pin.clone());
            pins.sort_by(|left, right| left.domain.cmp(&right.domain));
            require(
                material.pins == pins
                    && material.bundle.journal == journal
                    && material.bundle.grant_leaf == grant.hash()?,
                "pending team backup changed its exact signed delta",
            )?;
            let source_heads = material
                .source_pins
                .iter()
                .map(|pin| {
                    pin.minimum_head
                        .clone()
                        .ok_or(AuthorizationError::InvalidProof(
                            "pending team backup has no exact source head",
                        ))
                })
                .collect::<Result<Vec<_>>>()?;
            Ok(PendingTeamRecoveryMaterial {
                status: material.status.clone(),
                pending_request: material.pending_request.clone(),
                source_pins: material.source_pins.clone(),
                source_heads,
                source_grant: material.source_bundle.grant_leaf.clone(),
                format: material.format.clone(),
                version: material.version,
                root_secret_hex: material.root_secret_hex.clone(),
                owner_account: material.owner_account.clone(),
                team: material.team.clone(),
                recovery_public_key: material.recovery_public_key.clone(),
            })
        }
    }
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen(js_name = restorePreparedTeamCreation))]
pub fn restore_prepared_team_creation(
    material_json: &str,
    client: &Identity,
    authorization: &VerifiedClientAuthorization,
    now_ms: f64,
) -> crate::Result<PreparedTeamCreation> {
    let _signature_scope = ScopedSignatureVerification::new();
    let restore = || -> Result<_> {
        let now = time(now_ms)?;
        let (current, _) = load(authorization, &PublicKey::of(client), now)?;
        let material = pending_material(material_json, &current)?;
        let request = &material.pending_request.payload;
        if material
            .source_heads
            .iter()
            .any(|head| !current.state().roots.contains_key(head))
            || current.state().grant(&material.source_grant).is_none()
        {
            return Err(AuthorizationError::NeedPolicySync(
                material.source_heads.clone(),
            ));
        }
        material.pending_request.verify_signature()?;
        let root = Identity::from_secret_hex(&material.root_secret_hex)
            .map_err(|_| AuthorizationError::InvalidProof("invalid pending team root"))?;
        require(
            material.status == "pending"
                && material.format == "vibeface-team-recovery"
                && material.version == AUTH_VERSION
                && material.owner_account == request.owner_account
                && material.team == request.team_project.space
                && PublicKey::of(&root) == request.team_pin.genesis_root
                && PublicKey::of(client) == request.governor_device_public_key
                && material.pending_request.issuer == PublicKey::of(client)
                && authorization.scope.account_id == request.owner_account
                && authorization.scope.space_id == request.source_project.space
                && authorization.resources[0].payload.resource == request.source_project.resource
                && request.issued_at_ms <= now
                && material.source_pins.len() == material.source_heads.len()
                && material
                    .source_pins
                    .windows(2)
                    .all(|pair| pair[0].domain < pair[1].domain)
                && material
                    .source_heads
                    .iter()
                    .zip(&material.source_pins)
                    .all(|(head, pin)| head.domain == pin.domain)
                && material.source_pins.iter().all(|pin| {
                    current.pins().iter().any(|trusted| {
                        trusted.domain == pin.domain && trusted.genesis_root == pin.genesis_root
                    })
                })
                && material
                    .source_heads
                    .iter()
                    .all(|head| current.state().roots.contains_key(head))
                && request
                    .expected_heads
                    .iter()
                    .all(|head| material.source_heads.contains(head)),
            "pending team differs from independently saved account trust",
        )?;
        let pending = VerifiedPendingTeamCreation {
            signed: material.pending_request.clone(),
            source_heads: material.source_heads.clone(),
        };
        let before = current.before_team_creation(&pending)?;
        let verified = verify_team_creation_request(
            &before,
            &material.source_grant,
            &material.source_pins,
            &material.pending_request,
            request.issued_at_ms,
        )?;
        let Some(JournalEntry::SpaceGenesis(genesis)) = request.journal_delta.first() else {
            return Err(AuthorizationError::InvalidProof(
                "missing pending team genesis",
            ));
        };
        require(
            genesis.payload.recovery.keys == [material.recovery_public_key.clone()],
            "pending team backup changed its recovery key",
        )?;
        let approved = VerifiedTeamCreationDelta {
            signed: material.pending_request.clone(),
            source_heads: current.heads().to_vec(),
        };
        current.with_created_team(&approved)?;
        let mut usage = execution_handoff::ExecutionProofUsage::default();
        record_team_usage(&mut usage, &material.pending_request, &verified)?;
        usage.record_metadata(&material)?;
        finish_usage(&mut usage)?;
        Ok(PreparedTeamCreation {
            request: material.pending_request.clone(),
            source: current,
            source_grant: material.source_grant.clone(),
            original_heads: material.source_heads.clone(),
            approved,
            root_secret_hex: material.root_secret_hex.clone(),
            usage,
        })
    };
    restore().map_err(|error| error.to_string())
}

fn record_team_usage(
    usage: &mut execution_handoff::ExecutionProofUsage,
    signed: &Signed<TeamCreationRequest>,
    verified: &VerifiedTeamCreationRequest,
) -> Result<()> {
    usage.record_signed(signed)?;
    usage.record_budget(&verified.budget)?;
    for entry in &signed.payload.journal_delta {
        match entry {
            JournalEntry::SpaceGenesis(value) | JournalEntry::Space(value) => {
                usage.record_signed(value)?
            }
            JournalEntry::Resource(value) => usage.record_signed(value)?,
            JournalEntry::Membership(value) => usage.record_signed(value)?,
            JournalEntry::Grant(value) => usage.record_signed(value)?,
            _ => {
                return Err(AuthorizationError::InvalidProof(
                    "unexpected team delta object",
                ));
            }
        }
    }
    usage.record_metadata(signed)?;
    Ok(())
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen)]
pub struct VerifiedTeamCreation {
    snapshot: VerifiedPolicySnapshot,
    authorization: Option<VerifiedClientAuthorization>,
    project: ResourceId,
    recovery_material: String,
    usage: execution_handoff::ExecutionProofUsage,
}
impl Drop for VerifiedTeamCreation {
    fn drop(&mut self) {
        self.recovery_material.zeroize();
    }
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen(js_name = finishTeamCreation))]
pub fn finish_team_creation(
    prepared: &PreparedTeamCreation,
    acceptance_json: &str,
    now_ms: f64,
) -> crate::Result<VerifiedTeamCreation> {
    let _signature_scope = ScopedSignatureVerification::new();
    let finish = || -> Result<_> {
        let now = time(now_ms)?;
        let accepted: Signed<TeamCreationAcceptance> = decode_signed(acceptance_json.as_bytes())?;
        accepted.verify_signature()?;
        let accepted_at = accepted.payload.accepted_at_ms;
        require(accepted_at <= now, "team acceptance is from the future")?;
        let pending = VerifiedPendingTeamCreation {
            signed: prepared.request.clone(),
            source_heads: prepared.original_heads.clone(),
        };
        let before = prepared.source.before_team_creation(&pending)?;
        let verified = verify_team_creation_request(
            &before,
            &prepared.source_grant,
            prepared.source.pins(),
            &prepared.request,
            accepted_at,
        )?;
        let request = &prepared.request.payload;
        let value = &accepted.payload;
        require(
            value.version == AUTH_VERSION
                && accepted.issuer == request.target_node_public_key
                && value.target_node_public_key == request.target_node_public_key
                && value.request_hash == verified.request_hash
                && value.space == request.team_project.space
                && value.owner_account == request.owner_account
                && value.account_head == verified.account_head
                && value.nonce == request.nonce
                && value.accepted_at_ms >= request.issued_at_ms
                && value.expires_at_ms == request.expires_at_ms,
            "team acceptance differs from its exact prepared request",
        )?;
        validate_window(value.accepted_at_ms, value.expires_at_ms, accepted_at)?;
        let snapshot = prepared.source.with_created_team(&prepared.approved)?;
        let mut pins = snapshot.pins().to_vec();
        for pin in &mut pins {
            pin.minimum_head = snapshot.state().head(&pin.domain).cloned();
        }
        let snapshot = snapshot.with_independent_pins(&pins)?;
        let authorization = selected_authorization(
            &snapshot,
            &verified.grant,
            &request.owner_account,
            &request.governor_device_public_key,
            &request.team_project,
            now,
        );
        let authorization = match authorization {
            Ok(current) => Some(current),
            Err(AuthorizationError::BudgetExceeded) => {
                return Err(AuthorizationError::BudgetExceeded);
            }
            Err(_) => None,
        };
        let JournalEntry::SpaceGenesis(genesis) = &request.journal_delta[0] else {
            return Err(AuthorizationError::InvalidProof(
                "missing verified team genesis",
            ));
        };
        let recovery_material = json(&TeamRecoveryMaterial {
            status: "committed",
            pending_request: &prepared.request,
            source_pins: prepared.source.pins(),
            source_heads: &prepared.original_heads,
            source_grant: &prepared.source_grant,
            format: "vibeface-team-recovery",
            version: AUTH_VERSION,
            root_secret_hex: &prepared.root_secret_hex,
            owner_account: &request.owner_account,
            team: &request.team_project.space,
            recovery_public_key: &genesis.payload.recovery.keys[0],
        })
        .map_err(|_| AuthorizationError::InvalidProof("team private material encoding failed"))?;
        require(
            recovery_material.len() <= MAX_PROOF_BYTES,
            "team private material exceeds its carrier",
        )?;
        let mut usage = execution_handoff::ExecutionProofUsage::default();
        record_team_usage(&mut usage, &prepared.request, &verified)?;
        usage.record_signed(&accepted)?;
        if let Some(current) = &authorization {
            usage.record_budget(&current.budget)?;
        }
        finish_usage(&mut usage)?;
        Ok(VerifiedTeamCreation {
            snapshot,
            authorization,
            project: request.team_project.resource.clone(),
            recovery_material,
            usage,
        })
    };
    finish().map_err(|error| error.to_string())
}
#[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen)]
impl VerifiedTeamCreation {
    pub fn authorization(&self) -> Option<VerifiedClientAuthorization> {
        self.authorization.clone()
    }
    #[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen(js_name = pinsJson))]
    pub fn pins_json(&self) -> crate::Result<String> {
        json(&self.snapshot.pins())
    }
    #[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen(js_name = scopeJson))]
    pub fn scope_json(&self) -> crate::Result<String> {
        json(&self.authorization.as_ref().map(|value| value.scope()))
    }
    #[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen(js_name = projectResourceId))]
    pub fn project_resource_id(&self) -> String {
        self.project.to_string()
    }
    #[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen(js_name = recoveryMaterialJson))]
    pub fn recovery_material_json(&self) -> String {
        self.recovery_material.clone()
    }
    #[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen(js_name = proofUsageJson))]
    pub fn proof_usage_json(&self) -> crate::Result<String> {
        json(&self.usage.counts())
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::authorization::bootstrap::*;

    const NOW: u64 = 20_000;
    const END: u64 = 500_000;
    fn key(byte: u8) -> Identity {
        Identity::from_secret_hex(&hex::encode([byte; 32])).unwrap()
    }
    pub(crate) fn confirmed_authorization(
        state: &VerifiedPolicySet,
        pins: &[PolicyPin],
        bundle: &ClientAuthorizationBundle,
        resource: &ResourceRef,
        own: &Identity,
        node: &Identity,
        now: u64,
    ) -> VerifiedClientAuthorization {
        use client_policy_import::*;
        let domains: BTreeSet<_> = verify_client_policy_state_selected(
            state.clone(),
            bundle,
            pins,
            match &state
                .find_device(&DeviceId::from_key(&PublicKey::of(own)))
                .unwrap()
                .payload
                .subject
            {
                Actor::Account(account) => account.clone(),
                _ => unreachable!(),
            },
            resource.space.clone(),
            PublicKey::of(own),
            Some(resource),
            now,
        )
        .unwrap()
        .heads
        .into_iter()
        .map(|head| head.domain)
        .collect();
        let pins: Vec<_> = pins
            .iter()
            .filter(|pin| domains.contains(&pin.domain))
            .cloned()
            .collect();
        let bundle = ClientAuthorizationBundle {
            grant_leaf: bundle.grant_leaf.clone(),
            journal: bundle
                .journal
                .iter()
                .filter(|entry| {
                    let domain = match entry {
                        JournalEntry::AccountGenesis(value) | JournalEntry::Account(value) => {
                            PolicyDomain::Account(value.payload.account.clone())
                        }
                        JournalEntry::AccountCommit(value) => {
                            PolicyDomain::Account(value.proposal.payload.account.clone())
                        }
                        JournalEntry::SpaceGenesis(value) | JournalEntry::Space(value) => {
                            PolicyDomain::Space(value.payload.space.clone())
                        }
                        JournalEntry::SpaceCommit(value) => {
                            PolicyDomain::Space(value.proposal.payload.space.clone())
                        }
                        JournalEntry::Device(value) => value.payload.owner.clone(),
                        JournalEntry::Resource(value) => {
                            PolicyDomain::Space(value.payload.space.clone())
                        }
                        JournalEntry::Authenticator(value) => {
                            PolicyDomain::Account(value.payload.account.clone())
                        }
                        JournalEntry::Membership(value) => {
                            PolicyDomain::Space(value.payload.space.clone())
                        }
                        JournalEntry::Grant(value) => {
                            PolicyDomain::Space(value.payload.owner_space.clone())
                        }
                    };
                    domains.contains(&domain)
                })
                .cloned()
                .collect(),
        };
        let pins = pins.as_slice();
        let audience = state
            .grant(&bundle.grant_leaf)
            .unwrap()
            .payload
            .audience
            .clone();
        let account = match &state
            .find_device(&DeviceId::from_key(&PublicKey::of(own)))
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
            own,
            &node.public_key_hex(),
            &json(&pins).unwrap(),
            &json(&Some(&selection)).unwrap(),
            now as f64,
            now as f64 + 60_000.0,
        )
        .unwrap();
        let request: Signed<ClientPolicyReadRequest> = parse(&request_json).unwrap();
        let current = verify_client_policy_read(
            state,
            &request,
            &PublicKey::of(node),
            &PublicKey::of(own),
            now as u64,
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
                source_node_public_key: PublicKey::of(node),
                issued_at_ms: now as u64,
                expires_at_ms: now as u64 + 60_000,
            },
            node,
        )
        .unwrap();
        let mut import = begin_client_policy_import(
            &json(&pins).unwrap(),
            &request_json,
            &json(&snapshot).unwrap(),
            own,
            &node.public_key_hex(),
            now as f64,
        )
        .unwrap();
        let mut previous = snapshot.hash().unwrap();
        for (index, entry) in bundle.journal.iter().enumerate() {
            let complete = index + 1 == bundle.journal.len();
            let page_request: ClientPolicyPageRequest =
                serde_json::from_str(&import.page_request_json().unwrap()).unwrap();
            let page = Signed::sign(
                ClientPolicyPage {
                    version: AUTH_VERSION,
                    request_hash: request.hash().unwrap(),
                    snapshot_id: snapshot.payload.snapshot_id.clone(),
                    target_heads_hash: client_policy_target_heads_hash(current.heads()).unwrap(),
                    page_index: index as u64,
                    request_cursor: page_request.cursor,
                    previous_page_hash: previous,
                    entry: Some(entry.clone()),
                    next_cursor: if complete {
                        None
                    } else {
                        Some(ObjectHash::body(&("native-test-cursor", index)).unwrap())
                    },
                    complete,
                },
                node,
            )
            .unwrap();
            previous = page.hash().unwrap();
            import
                .append_page(&json(&page).unwrap(), now as f64)
                .unwrap();
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
                source_node_public_key: PublicKey::of(node),
                issued_at_ms: now as u64,
                expires_at_ms: now as u64 + 60_000,
            },
            node,
        )
        .unwrap();
        let authorization = import
            .finish(
                &json(&witness).unwrap(),
                &json(&selection).unwrap(),
                now as f64,
            )
            .unwrap();
        let confirmation_request_json = build_client_policy_auth_confirm_request(
            &authorization,
            own,
            now as f64,
            now as f64 + 30_000.0,
        )
        .unwrap();
        let confirmation_request: Signed<ClientPolicyAuthConfirmRequest> =
            parse(&confirmation_request_json).unwrap();
        verify_client_policy_auth_confirm(
            state,
            &request,
            &snapshot,
            &witness,
            &confirmation_request,
            &PublicKey::of(node),
            &PublicKey::of(own),
            now as u64,
        )
        .unwrap();
        let confirmation = Signed::sign(
            ClientPolicyAuthConfirmation {
                version: AUTH_VERSION,
                context: confirmation_request.payload.context.clone(),
                confirmation_request_hash: confirmation_request.hash().unwrap(),
                selection,
                target_heads: current.heads().to_vec(),
                device_public_key: PublicKey::of(own),
                source_node_public_key: PublicKey::of(node),
                accepted_at_ms: now as u64,
            },
            node,
        )
        .unwrap();
        confirm_client_policy_authorization(
            &authorization,
            &confirmation_request_json,
            &json(&confirmation).unwrap(),
            &node.public_key_hex(),
            now as f64,
        )
        .unwrap()
    }
    struct Fixture {
        pins: Vec<PolicyPin>,
        bundle: ClientAuthorizationBundle,
        account: AccountId,
        project: ResourceId,
    }
    impl Fixture {
        fn personal() -> Self {
            let nonce = Nonce::from_bytes([9; 32]).to_string();
            let claim_json = create_node_claim(&key(4), &nonce, 10_000.0, 100_000.0).unwrap();
            let claim =
                verify_node_claim(&claim_json, &key(4).public_key_hex(), &nonce, 10_000.0).unwrap();
            let prepared =
                prepare_account_creation(&key(1), &key(2), &key(3), &claim, 1_000_000.0, 10_000.0)
                    .unwrap();
            let proof = sign_node_possession(
                &prepared.node_request_json().unwrap(),
                &key(4),
                &nonce,
                10_000.0,
            )
            .unwrap();
            let created = finish_account_creation(&prepared, &key(1), &proof, 10_000.0).unwrap();
            let bundle: ClientAuthorizationBundle = parse(&created.bundle_json().unwrap()).unwrap();
            let scope: ClientScope = parse(&created.scope_json().unwrap()).unwrap();
            let project = bundle
                .journal
                .iter()
                .find_map(|entry| match entry {
                    JournalEntry::Resource(value)
                        if value.payload.kind == ResourceKind::Project =>
                    {
                        Some(value.payload.resource.clone())
                    }
                    _ => None,
                })
                .unwrap();
            Self {
                pins: parse(&created.pins_json().unwrap()).unwrap(),
                bundle,
                account: scope.account_id,
                project,
            }
        }
        fn state(&self) -> VerifiedPolicySet {
            replay_policies(&self.pins, &self.bundle.journal).unwrap()
        }
        fn pins_json(&self) -> String {
            json(&self.pins).unwrap()
        }
        fn bundle_json(&self) -> String {
            json(&self.bundle).unwrap()
        }
        fn authorization(&self) -> VerifiedClientAuthorization {
            confirmed_authorization(
                &self.state(),
                &self.pins,
                &self.bundle,
                &ResourceRef {
                    space: self
                        .state()
                        .grant(&self.bundle.grant_leaf)
                        .unwrap()
                        .payload
                        .owner_space
                        .clone(),
                    resource: self.project.clone(),
                },
                &key(3),
                &key(4),
                NOW,
            )
        }
        fn prepare_team(&self, root: &Identity) -> crate::Result<PreparedTeamCreation> {
            prepare_team_creation(
                root,
                &key(3),
                self.account.as_str(),
                &key(4).public_key_hex(),
                &key(2).public_key_hex(),
                self.project.as_str(),
                &self.authorization(),
                END as f64,
                NOW as f64,
            )
        }
        fn team(&self) -> (Self, VerifiedTeamCreation) {
            let prepared = self.prepare_team(&key(11)).unwrap();
            let acceptance = accept_team_creation(
                &prepared.request_json().unwrap(),
                &key(4),
                &self.authorization(),
                NOW as f64,
            )
            .unwrap();
            let raw_acceptance = accept_team_creation_in_state(
                &self.state(),
                &self.bundle.grant_leaf,
                &self.pins,
                &prepared.request,
                &key(4),
                NOW,
            )
            .unwrap();
            assert_eq!(
                parse::<Signed<TeamCreationAcceptance>>(&acceptance).unwrap(),
                raw_acceptance
            );
            let pending = prepared.recovery_material_json().unwrap();
            let pending_value: serde_json::Value = parse(&pending).unwrap();
            assert_eq!(pending_value["status"], "pending");
            let mut old_bundle = self.bundle.clone();
            old_bundle
                .journal
                .extend(prepared.request.payload.journal_delta.clone());
            let JournalEntry::Grant(team_grant) = &prepared.request.payload.journal_delta[3] else {
                unreachable!()
            };
            old_bundle.grant_leaf = team_grant.hash().unwrap();
            let mut old_pins = self.pins.clone();
            old_pins.push(prepared.request.payload.team_pin.clone());
            old_pins.sort_by(|left, right| left.domain.cmp(&right.domain));
            let earlier = EarlierPendingTeamRecoveryMaterial {
                status: "pending".to_owned(),
                pending_request: prepared.request.clone(),
                source_pins: self.pins.clone(),
                source_bundle: self.bundle.clone(),
                format: "vibeface-team-recovery".to_owned(),
                version: AUTH_VERSION,
                root_secret_hex: key(11).secret_hex(),
                owner_account: self.account.clone(),
                team: prepared.request.payload.team_project.space.clone(),
                recovery_public_key: PublicKey::of(&key(2)),
                pins: old_pins,
                bundle: old_bundle,
            };
            let restored_earlier = restore_prepared_team_creation(
                &json(&earlier).unwrap(),
                &key(3),
                &self.authorization(),
                NOW as f64,
            )
            .unwrap();
            assert_eq!(
                restored_earlier.request_json().unwrap(),
                prepared.request_json().unwrap()
            );
            let mut changed_earlier = serde_json::to_value(&earlier).unwrap();
            changed_earlier["source_bundle"]["journal"][0]["value"]["payload"]["sequence"] =
                999.into();
            assert!(
                restore_prepared_team_creation(
                    &changed_earlier.to_string(),
                    &key(3),
                    &self.authorization(),
                    NOW as f64
                )
                .is_err()
            );
            let restored = restore_prepared_team_creation(
                &pending,
                &key(3),
                &self.authorization(),
                (NOW + INVOCATION_MAX_AGE_MS + 1) as f64,
            )
            .unwrap();
            assert_eq!(
                restored.request_json().unwrap(),
                prepared.request_json().unwrap()
            );
            assert_eq!(
                finish_team_creation(&restored, &acceptance, NOW as f64)
                    .unwrap()
                    .pins_json()
                    .unwrap(),
                finish_team_creation(&prepared, &acceptance, NOW as f64)
                    .unwrap()
                    .pins_json()
                    .unwrap()
            );
            for field in ["root_secret_hex", "team", "status", "owner_account"] {
                let mut altered = pending_value.clone();
                altered[field] = "invalid".into();
                assert!(
                    restore_prepared_team_creation(
                        &altered.to_string(),
                        &key(3),
                        &self.authorization(),
                        NOW as f64
                    )
                    .is_err()
                );
            }
            assert!(
                restore_prepared_team_creation(
                    &pending,
                    &key(12),
                    &self.authorization(),
                    NOW as f64
                )
                .is_err()
            );
            assert_eq!(
                pending_value["pending_request"],
                serde_json::to_value(&prepared.request).unwrap()
            );
            let result = finish_team_creation(&prepared, &acceptance, NOW as f64).unwrap();
            let delayed = finish_team_creation(
                &prepared,
                &acceptance,
                (NOW + INVOCATION_MAX_AGE_MS + 1) as f64,
            )
            .unwrap();
            assert_eq!(delayed.pins_json().unwrap(), result.pins_json().unwrap());
            let expired = finish_team_creation(&prepared, &acceptance, (END + 1) as f64).unwrap();
            assert_eq!(expired.pins_json().unwrap(), result.pins_json().unwrap());
            assert_eq!(expired.scope_json().unwrap(), "null");
            assert!(finish_team_creation(&prepared, &acceptance, (NOW - 1) as f64).is_err());
            (
                Self {
                    pins: result.snapshot.pins().to_vec(),
                    bundle: ClientAuthorizationBundle {
                        journal: result.snapshot.journal().to_vec(),
                        grant_leaf: result.authorization.as_ref().unwrap().grant_leaf.clone(),
                    },
                    account: self.account.clone(),
                    project: result.project.clone(),
                },
                result,
            )
        }
        fn commit(&self, prepared: &PreparedSpaceGovernanceChange) -> Self {
            let request = prepared.request_json().unwrap();
            let challenge_json = issue_space_governance_challenge(
                &request,
                &key(4),
                &self.authorization(),
                NOW as f64,
            )
            .unwrap();
            let challenge: Signed<PolicyCommit> = parse(&challenge_json).unwrap();
            assert_eq!(
                issue_space_governance_challenge_in_state(
                    &self.state(),
                    &prepared.request,
                    &key(4),
                    NOW
                )
                .unwrap(),
                challenge
            );
            let receipts: Vec<_> = challenge
                .payload
                .dependencies
                .iter()
                .map(|head| {
                    Signed::sign(
                        FreshnessReceipt {
                            version: AUTH_VERSION,
                            current: head.clone(),
                            challenge_hash: challenge.challenge_hash().unwrap(),
                            issued_at_ms: NOW,
                            expires_at_ms: challenge.payload.expires_at_ms,
                        },
                        &key(4),
                    )
                    .unwrap()
                })
                .collect();
            assert!(
                accept_space_governance_change(
                    &request,
                    &challenge_json,
                    "[]",
                    &key(4),
                    &self.authorization(),
                    NOW as f64
                )
                .is_err()
            );
            let committed = accept_space_governance_change(
                &request,
                &challenge_json,
                &json(&receipts).unwrap(),
                &key(4),
                &self.authorization(),
                NOW as f64,
            )
            .unwrap();
            let raw_committed = accept_space_governance_change_in_state(
                &self.state(),
                &prepared.request,
                &challenge,
                &receipts,
                &key(4),
                NOW,
            )
            .unwrap();
            assert_eq!(
                parse::<Committed<SpaceDocument>>(&committed).unwrap(),
                raw_committed
            );
            let result = finish_space_governance_change(prepared, &committed, NOW as f64).unwrap();
            let delayed = finish_space_governance_change(
                prepared,
                &committed,
                (NOW + INVOCATION_MAX_AGE_MS + 1) as f64,
            )
            .unwrap();
            assert_eq!(delayed.pins_json().unwrap(), result.pins_json().unwrap());
            let expired =
                finish_space_governance_change(prepared, &committed, (END + 1) as f64).unwrap();
            assert_eq!(expired.pins_json().unwrap(), result.pins_json().unwrap());
            assert_eq!(expired.scope_json().unwrap(), "null");
            assert!(
                finish_space_governance_change(prepared, &committed, (NOW - 1) as f64).is_err()
            );
            let next = Self {
                pins: result.snapshot.pins().to_vec(),
                bundle: ClientAuthorizationBundle {
                    journal: result.snapshot.journal().to_vec(),
                    grant_leaf: self.bundle.grant_leaf.clone(),
                },
                account: self.account.clone(),
                project: self.project.clone(),
            };
            next.state();
            next
        }
    }

    #[test]
    fn governance_consumes_confirmed_native_history_and_preserves_team_delta_identity() {
        let mut personal = Fixture::personal();
        let mut state = personal.state();
        let space = SpaceId::personal(&personal.account);
        for _ in 0..124 {
            let mut document = state.space(&space).unwrap().clone();
            document.previous = state.head(&PolicyDomain::Space(space.clone())).cloned();
            document.sequence += 1;
            let signed = Signed::sign(document, &key(1)).unwrap();
            let head = state.install_space(signed.clone()).unwrap();
            personal.bundle.journal.push(JournalEntry::Space(signed));
            personal
                .pins
                .iter_mut()
                .find(|pin| pin.domain == head.domain)
                .unwrap()
                .minimum_head = Some(head.clone());
        }
        assert!(personal.bundle.journal.len() > 122);
        let confirmed = personal.authorization();
        let prepared = prepare_team_creation(
            &key(11),
            &key(3),
            personal.account.as_str(),
            &key(4).public_key_hex(),
            &key(2).public_key_hex(),
            personal.project.as_str(),
            &confirmed,
            END as f64,
            NOW as f64,
        )
        .unwrap();
        let counts = prepared.usage.counts();
        assert!(counts.authorization_bytes <= MAX_PROOF_BYTES);
        assert!(counts.unique_signed_objects <= MAX_PROOF_OBJECTS);
        assert!(counts.actual.unwrap().actual_verifications <= MAX_PROOF_SIGNATURES);
        let mut legacy = confirmed.clone();
        legacy.client_policy_context = None;
        assert!(
            prepare_team_creation(
                &key(11),
                &key(3),
                personal.account.as_str(),
                &key(4).public_key_hex(),
                &key(2).public_key_hex(),
                personal.project.as_str(),
                &legacy,
                END as f64,
                NOW as f64
            )
            .is_err()
        );

        let created = prepared
            .source
            .with_created_team(&prepared.approved)
            .unwrap();
        let team = &prepared.request.payload.team_project;
        assert!(created.state().space(&team.space).is_some());
        let mut grant = created
            .state()
            .grant(&match &prepared.request.payload.journal_delta[3] {
                JournalEntry::Grant(value) => value.hash().unwrap(),
                _ => unreachable!(),
            })
            .unwrap()
            .payload
            .clone();
        grant.grantee = Grantee::Actor(Actor::Account(personal.account.clone()));
        grant.scope = ResourceScope::Exact(team.clone());
        grant.actions = vec![Action::ConversationRead];
        grant.remaining_delegations = 0;
        grant.issued_under = created
            .state()
            .head(&PolicyDomain::Space(team.space.clone()))
            .unwrap()
            .clone();
        grant.nonce = Nonce::random().unwrap();
        let grant = Signed::sign(grant, &key(11)).unwrap();
        let future_grant = grant.hash().unwrap();
        let mut document = created.state().space(&team.space).unwrap().clone();
        document.previous = created
            .state()
            .head(&PolicyDomain::Space(team.space.clone()))
            .cloned();
        document.sequence += 1;
        document.grants.push(future_grant.clone());
        document.grants.sort();
        let newer = created
            .apply_delta(&[
                JournalEntry::Grant(grant),
                JournalEntry::Space(Signed::sign(document, &key(11)).unwrap()),
            ])
            .unwrap();
        let original_retry = newer.with_created_team(&prepared.approved).unwrap();
        assert_eq!(original_retry.heads(), newer.heads());
        assert_eq!(original_retry.journal(), newer.journal());
        let mut incomplete = prepared.approved.clone();
        incomplete.source_heads.pop();
        assert!(newer.with_created_team(&incomplete).is_err());
        let mut forged = prepared.approved.clone();
        forged.source_heads[0].sequence += 999;
        assert!(newer.with_created_team(&forged).is_err());
        let mut approved = prepared.approved.clone();
        approved.source_heads = newer.heads().to_vec();
        let repeated = newer.with_created_team(&approved).unwrap();
        assert_eq!(repeated.heads(), newer.heads());
        assert_eq!(repeated.journal(), newer.journal());
        let pending = VerifiedPendingTeamCreation {
            signed: prepared.request.clone(),
            source_heads: prepared.original_heads.clone(),
        };
        let before = newer.before_team_creation(&pending).unwrap();
        assert!(before.space(&team.space).is_none());
        assert!(before.grant(&future_grant).is_none());
        let mut missing = VerifiedPendingTeamCreation {
            signed: prepared.request.clone(),
            source_heads: prepared.original_heads.clone(),
        };
        missing.source_heads[0].sequence += 999;
        assert!(newer.before_team_creation(&missing).is_err());
    }

    #[test]
    fn team_creation_references_the_personal_node_and_preserves_private_recovery_material() {
        let personal = Fixture::personal();
        let (team, result) = personal.team();
        let state = team.state();
        let space = &result.authorization.as_ref().unwrap().scope().space_id;
        let document = state.space(space).unwrap();
        assert_eq!(document.current_root, PublicKey::of(&key(11)));
        assert_eq!(document.recovery.keys, [PublicKey::of(&key(2))]);
        let certificate = state
            .verify_service_device(space, &DeviceId::from_key(&PublicKey::of(&key(4))), NOW)
            .unwrap();
        assert_eq!(
            certificate.payload.owner,
            PolicyDomain::Account(personal.account.clone())
        );
        assert_ne!(result.project, personal.project);
        assert!(!result.pins_json().unwrap().contains("secret_hex"));
        assert!(!result.recovery_material_json().contains("source_bundle"));
        let material = result.recovery_material_json();
        let value: serde_json::Value = serde_json::from_str(&material).unwrap();
        let restored_root =
            Identity::from_secret_hex(value["root_secret_hex"].as_str().unwrap()).unwrap();
        assert_eq!(restored_root.public_key_hex(), key(11).public_key_hex());
        let sealed =
            crate::recovery_seal(material.as_bytes(), "team fixture recovery password").unwrap();
        assert!(
            crate::recovery_open(&sealed, "team fixture recovery password").unwrap()
                == material.as_bytes()
        );
    }

    #[test]
    fn daily_membership_directional_grant_resource_and_revocation_use_real_commits() {
        let (team, result) = Fixture::personal().team();
        let space = result
            .authorization
            .as_ref()
            .unwrap()
            .scope()
            .space_id
            .clone();
        let guest = AccountId::from_root(&PublicKey::of(&key(12)));
        let member = SpaceMembershipIntent {
            space: space.clone(),
            subject_account: guest.clone(),
            roles: vec![SpaceRole::Viewer],
            expires_at_ms: END,
        };
        let prepared = prepare_space_membership_change(
            &json(&member).unwrap(),
            &key(3),
            &team.authorization(),
            NOW as f64,
        )
        .unwrap();
        let team = team.commit(&prepared);
        assert!(
            team.state()
                .space(&space)
                .unwrap()
                .memberships
                .iter()
                .any(|hash| team
                    .state()
                    .membership(hash)
                    .is_some_and(|value| value.payload.subject == Actor::Account(guest.clone())))
        );
        let grant_intent = DirectionalGrantIntent {
            space: space.clone(),
            grantee: Grantee::Actor(Actor::Account(team.account.clone())),
            scope: ResourceScope::Exact(ResourceRef {
                space: space.clone(),
                resource: team.project.clone(),
            }),
            actions: vec![Action::ConversationRead],
            audience_node_public_key: PublicKey::of(&key(4)),
            constraints: GrantConstraints::default(),
            remaining_delegations: 1,
            expires_at_ms: END,
        };
        let prepared = prepare_directional_grant(
            &json(&grant_intent).unwrap(),
            &key(3),
            &team.authorization(),
            NOW as f64,
        )
        .unwrap();
        let JournalEntry::Grant(grant) = &prepared.request.new_objects[0] else {
            panic!("missing directional grant")
        };
        assert!(grant.payload.execution_source.is_none());
        let mut untrusted_source = serde_json::to_value(&grant_intent).unwrap();
        untrusted_source["execution_source"] = serde_json::json!({"pins": []});
        assert!(
            prepare_directional_grant(
                &json(&untrusted_source).unwrap(),
                &key(3),
                &team.authorization(),
                NOW as f64,
            )
            .is_err()
        );
        let child = grant.hash().unwrap();
        let team = team.commit(&prepared);
        let mut recipient_bundle = team.bundle.clone();
        recipient_bundle.grant_leaf = child.clone();
        assert!(
            verify_client_authorization(
                &json(&recipient_bundle).unwrap(),
                &team.pins_json(),
                team.account.as_str(),
                space.as_str(),
                &key(3).public_key_hex(),
                NOW as f64
            )
            .is_ok()
        );
        let resource = SpaceResourceIntent {
            space: space.clone(),
            resource: ResourceId::random().unwrap(),
            kind: ResourceKind::Project,
            host_node_public_key: PublicKey::of(&key(4)),
        };
        let prepared = prepare_space_resource_registration(
            &json(&resource).unwrap(),
            &key(3),
            &team.authorization(),
            NOW as f64,
        )
        .unwrap();
        let team = team.commit(&prepared);
        assert!(team.state().resource(&space, &resource.resource).is_some());
        let prepared = prepare_space_grant_revocation(
            space.as_str(),
            child.as_str(),
            &key(3),
            &team.authorization(),
            NOW as f64,
        )
        .unwrap();
        let team = team.commit(&prepared);
        let mut revoked_bundle = team.bundle.clone();
        revoked_bundle.grant_leaf = child;
        assert!(
            verify_client_authorization(
                &json(&revoked_bundle).unwrap(),
                &team.pins_json(),
                team.account.as_str(),
                space.as_str(),
                &key(3).public_key_hex(),
                NOW as f64
            )
            .is_err()
        );
        let removal = SpaceMembershipIntent {
            roles: vec![],
            ..member
        };
        let prepared = prepare_space_membership_change(
            &json(&removal).unwrap(),
            &key(3),
            &team.authorization(),
            NOW as f64,
        )
        .unwrap();
        let team = team.commit(&prepared);
        assert!(
            !team
                .state()
                .space(&space)
                .unwrap()
                .memberships
                .iter()
                .any(|hash| team
                    .state()
                    .membership(hash)
                    .is_some_and(|value| value.payload.subject == Actor::Account(guest.clone())))
        );
    }

    #[test]
    fn bounded_member_selector_builders_follow_current_roles_and_both_policy_domains() {
        let (team, result) = Fixture::personal().team();
        let space = result
            .authorization
            .as_ref()
            .unwrap()
            .scope()
            .space_id
            .clone();
        let owner = verify_client_authorization(
            &team.bundle_json(),
            &team.pins_json(),
            team.account.as_str(),
            space.as_str(),
            &key(3).public_key_hex(),
            NOW as f64,
        )
        .unwrap();
        let recovery: RecoveryPolicy =
            parse(&owner.account_recovery_policy_json().unwrap()).unwrap();
        assert_eq!(recovery.keys, [PublicKey::of(&key(2))]);
        let genesis = ObjectHash::parse(&owner.space_genesis_hash_string()).unwrap();
        let intent = DirectionalGrantIntent {
            space: space.clone(),
            grantee: Grantee::MembersOf(MembersOf {
                space: space.clone(),
                pinned_genesis: genesis,
                kind: MemberKind::Account,
                roles_any: vec![SpaceRole::Viewer],
            }),
            scope: ResourceScope::Exact(ResourceRef {
                space: space.clone(),
                resource: team.project.clone(),
            }),
            actions: vec![Action::ConversationRead],
            audience_node_public_key: PublicKey::of(&key(4)),
            constraints: GrantConstraints::default(),
            remaining_delegations: 0,
            expires_at_ms: END,
        };
        for field in 0..6 {
            let mut bad = intent.clone();
            let Grantee::MembersOf(selector) = &mut bad.grantee else {
                unreachable!()
            };
            match field {
                0 => selector.kind = MemberKind::Service,
                1 => selector.space = SpaceId::personal(&team.account),
                2 => selector.pinned_genesis = ObjectHash::body(&"another team genesis").unwrap(),
                3 => selector.roles_any.clear(),
                4 => bad.scope = ResourceScope::WholeSpace(space.clone()),
                _ => bad.remaining_delegations = 1,
            }
            assert!(
                prepare_directional_grant(
                    &json(&bad).unwrap(),
                    &key(3),
                    &team.authorization(),
                    NOW as f64
                )
                .is_err()
            );
        }
        // The administrator need not be a Viewer to issue the bounded Viewer template.
        let prepared = prepare_directional_grant(
            &json(&intent).unwrap(),
            &key(3),
            &team.authorization(),
            NOW as f64,
        )
        .unwrap();
        let JournalEntry::Grant(grant) = &prepared.request.new_objects[0] else {
            panic!("missing selector grant")
        };
        let child = grant.hash().unwrap();
        let team = team.commit(&prepared);
        let mut recipient = team.bundle.clone();
        recipient.grant_leaf = child.clone();
        assert!(
            verify_client_authorization(
                &json(&recipient).unwrap(),
                &team.pins_json(),
                team.account.as_str(),
                space.as_str(),
                &key(3).public_key_hex(),
                NOW as f64
            )
            .is_err()
        );
        let membership = SpaceMembershipIntent {
            space: space.clone(),
            subject_account: team.account.clone(),
            roles: vec![SpaceRole::Viewer, SpaceRole::SpaceAdmin],
            expires_at_ms: END,
        };
        let prepared = prepare_space_membership_change(
            &json(&membership).unwrap(),
            &key(3),
            &team.authorization(),
            NOW as f64,
        )
        .unwrap();
        let team = team.commit(&prepared);
        let mut recipient = team.bundle.clone();
        recipient.grant_leaf = child.clone();
        let auth = verify_client_authorization(
            &json(&recipient).unwrap(),
            &team.pins_json(),
            team.account.as_str(),
            space.as_str(),
            &key(3).public_key_hex(),
            NOW as f64,
        )
        .unwrap();
        assert_eq!(auth.heads().len(), 2);
        assert!(
            auth.heads()
                .iter()
                .any(|head| head.domain == PolicyDomain::Account(team.account.clone()))
        );
        assert!(
            auth.heads()
                .iter()
                .any(|head| head.domain == PolicyDomain::Space(space.clone()))
        );
        let membership = SpaceMembershipIntent {
            roles: vec![SpaceRole::SpaceAdmin],
            ..membership
        };
        let prepared = prepare_space_membership_change(
            &json(&membership).unwrap(),
            &key(3),
            &team.authorization(),
            NOW as f64,
        )
        .unwrap();
        let team = team.commit(&prepared);
        let mut recipient = team.bundle.clone();
        recipient.grant_leaf = child;
        assert!(
            verify_client_authorization(
                &json(&recipient).unwrap(),
                &team.pins_json(),
                team.account.as_str(),
                space.as_str(),
                &key(3).public_key_hex(),
                NOW as f64
            )
            .is_err()
        );
    }

    #[test]
    fn creation_and_daily_changes_reject_root_reuse_wrong_nodes_and_widened_scope() {
        let personal = Fixture::personal();
        for byte in [1, 2, 3, 4] {
            assert!(personal.prepare_team(&key(byte)).is_err());
        }
        let prepared = personal.prepare_team(&key(11)).unwrap();
        assert!(
            accept_team_creation(
                &prepared.request_json().unwrap(),
                &key(5),
                &personal.authorization(),
                NOW as f64
            )
            .is_err()
        );
        for field in 0..3 {
            let mut request = prepared.request.payload.clone();
            match field {
                0 => request.team_project = request.source_project.clone(),
                1 => request.expected_heads[0].sequence += 1,
                _ => request.expires_at_ms = NOW + INVOCATION_MAX_AGE_MS + 1,
            }
            let signed = Signed::sign(request, &key(3)).unwrap();
            assert!(
                verify_team_creation_request(
                    &personal.state(),
                    &personal.bundle.grant_leaf,
                    &personal.pins,
                    &signed,
                    NOW
                )
                .is_err()
            );
        }
        let (team, result) = personal.team();
        let intent = SpaceMembershipIntent {
            space: result
                .authorization
                .as_ref()
                .unwrap()
                .scope()
                .space_id
                .clone(),
            subject_account: team.account.clone(),
            roles: vec![SpaceRole::Viewer],
            expires_at_ms: END,
        };
        assert!(
            prepare_space_membership_change(
                &json(&intent).unwrap(),
                &key(4),
                &team.authorization(),
                NOW as f64
            )
            .is_err()
        );
        let grant = DirectionalGrantIntent {
            space: result
                .authorization
                .as_ref()
                .unwrap()
                .scope()
                .space_id
                .clone(),
            grantee: Grantee::Actor(Actor::Account(team.account.clone())),
            scope: ResourceScope::WholeSpace(
                result
                    .authorization
                    .as_ref()
                    .unwrap()
                    .scope()
                    .space_id
                    .clone(),
            ),
            actions: vec![Action::ConversationRead],
            audience_node_public_key: PublicKey::of(&key(4)),
            constraints: GrantConstraints::default(),
            remaining_delegations: 8,
            expires_at_ms: END,
        };
        assert!(
            prepare_directional_grant(
                &json(&grant).unwrap(),
                &key(3),
                &team.authorization(),
                NOW as f64
            )
            .is_err()
        );
    }

    #[test]
    fn personal_owner_revocation_invalidates_a_team_node_reference() {
        let (team, result) = Fixture::personal().team();
        let mut state = team.state();
        let old = DeviceId::from_key(&PublicKey::of(&key(4)));
        let mut certificate = state.find_device(&old).unwrap().payload.clone();
        certificate.public_key = PublicKey::of(&key(5));
        certificate.device = DeviceId::from_key(&certificate.public_key);
        certificate.subject = Actor::Service(ServiceId::from_key(&certificate.public_key));
        certificate.issued_under = state
            .head(&PolicyDomain::Account(team.account.clone()))
            .unwrap()
            .clone();
        certificate.prove_possession(&key(5)).unwrap();
        let replacement = state
            .install_device(Signed::sign(certificate, &key(1)).unwrap(), NOW)
            .unwrap();
        let mut document = state.account(&team.account).unwrap().clone();
        document.previous = state
            .head(&PolicyDomain::Account(team.account.clone()))
            .cloned();
        document.authority_epoch += 1;
        document.sequence = 0;
        document.authority = AuthorityBinding::from_key(PublicKey::of(&key(5)));
        document.devices.retain(|hash| {
            state
                .device(hash)
                .is_none_or(|value| value.payload.device != old)
        });
        document.devices.push(replacement);
        document.devices.sort();
        document.revoked_devices.push(old.clone());
        document.revoked_devices.sort();
        state
            .install_account(Signed::sign(document, &key(1)).unwrap())
            .unwrap();
        assert!(
            state
                .verify_service_device(
                    &result.authorization.as_ref().unwrap().scope().space_id,
                    &old,
                    NOW
                )
                .is_err()
        );
    }
}
