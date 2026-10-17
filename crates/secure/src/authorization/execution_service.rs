//! Execution services require their physical owner's current personal root.

use super::{bootstrap::recovery::RecoveryMaterial, governance, *};

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

pub struct VerifiedExecutionServiceRegistrationRequest {
    expected_heads: Vec<PolicyStamp>,
    commit_heads: Vec<PolicyStamp>,
    expires: u64,
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
) -> Result<VerifiedExecutionServiceRegistrationRequest> {
    let personal = SpaceId::personal(account);
    state.verify_account_client_device(account, &DeviceId::from_key(signer), now)?;
    let mut budget = Budget::default();
    let mut dependencies = BTreeMap::new();
    require(
        state.actor_for_signer(signer, now, &mut budget, &mut dependencies)?
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
        &mut budget,
    )?;
    let target = state.verify_service_device(&personal, &DeviceId::from_key(node), now)?;
    let source_target =
        state.verify_service_device(&source.space, &DeviceId::from_key(node), now)?;
    require(
        target.payload.public_key == *node
            && target.payload.owner == PolicyDomain::Account(account.clone())
            && source_target.payload.public_key == *node
            && source_target.payload.owner == PolicyDomain::Account(account.clone()),
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
    state.validate_certificate(
        target,
        now,
        &mut budget,
        &mut dependencies,
        &mut BTreeSet::new(),
    )?;
    state.validate_certificate(
        source_target,
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
    state.dependency(&PolicyDomain::Space(personal.clone()), &mut dependencies)?;
    state.expand_authorities(now, &mut budget, &mut dependencies)?;
    let mut commit_budget = Budget::default();
    let mut commit_heads = BTreeMap::new();
    state.dependency(&PolicyDomain::Space(personal), &mut commit_heads)?;
    state.expand_authorities(now, &mut commit_budget, &mut commit_heads)?;
    Ok(VerifiedExecutionServiceRegistrationRequest {
        expected_heads: dependencies.into_values().collect(),
        commit_heads: commit_heads.into_values().collect(),
        expires: budget.expires_at_ms.min(commit_budget.expires_at_ms),
    })
}

pub fn verify_execution_service_registration(
    state: &VerifiedPolicySet,
    signed: &ExecutionServiceRegistrationRequest,
    local_node: &PublicKey,
    now: u64,
) -> Result<VerifiedExecutionServiceRegistrationRequest> {
    require(
        canonical(signed)?.len() <= MAX_PROOF_BYTES,
        "execution registration too large",
    )?;
    signed.intent.verify_signature()?;
    signed.root_approval.verify_signature()?;
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
    request.proposal.verify_signature()?;
    let verified = authorization(
        state,
        &intent.account,
        &signed.intent.issuer,
        &intent.source_project,
        &intent.source_grant,
        local_node,
        now,
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
    Ok(verified)
}

pub fn execution_service_registration_challenge(
    state: &VerifiedPolicySet,
    request: &ExecutionServiceRegistrationRequest,
    local_node: &PublicKey,
    now: u64,
) -> Result<FreshnessChallenge> {
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
) -> Result<()> {
    let verified = verify_execution_service_registration(state, request, local_node, now)?;
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
    let mut seen = BTreeSet::new();
    for receipt in receipts {
        receipt.verify_signature()?;
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

fn merged_context(
    material: &RecoveryMaterial,
    pins: Vec<PolicyPin>,
    bundle: ClientAuthorizationBundle,
) -> Result<(Vec<PolicyPin>, ClientAuthorizationBundle, VerifiedPolicySet)> {
    let saved = replay_policies(&material.pins, &material.bundle.journal)?;
    let source = replay_policies(&pins, &bundle.journal)?;
    let domain = PolicyDomain::Account(material.scope.account_id.clone());
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
    let mut combined: BTreeMap<_, _> = material
        .pins
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
        [&material.bundle.journal, &bundle.journal]
    } else {
        [&bundle.journal, &material.bundle.journal]
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
    for pin in &material.pins {
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
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen(js_name = prepareExecutionServiceRegistration))]
pub fn prepare_execution_service_registration(
    source_project_json: &str,
    owner_material_json: &str,
    signer: &Identity,
    pins_json: &str,
    bundle_json: &str,
    target_node_public_key: &str,
    now_ms: f64,
    expires_at_ms: f64,
) -> crate::Result<PreparedExecutionServiceRegistration> {
    let prepare = || -> Result<_> {
        bootstrap::recovery::inspect_account_recovery_material(owner_material_json, now_ms)
            .map_err(|_| {
                AuthorizationError::InvalidProof("invalid execution owner's private material")
            })?;
        let material: RecoveryMaterial = parse(owner_material_json)?;
        let root = Identity::from_secret_hex(&material.root_secret_hex)
            .map_err(|_| AuthorizationError::InvalidProof("invalid execution owner root"))?;
        let source: ResourceRef = parse(source_project_json)?;
        let (pins, bundle, state) =
            merged_context(&material, parse(pins_json)?, parse(bundle_json)?)?;
        let account = material.scope.account_id.clone();
        require(
            state
                .account(&account)
                .is_some_and(|account| account.current_root == PublicKey::of(&root)),
            "execution owner's material is not its current root",
        )?;
        let now = bootstrap::timestamp(now_ms)?;
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
        )?;
        let personal = SpaceId::personal(&account);
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
        verify_execution_service_registration(&state, &request, &target, now)?;
        Ok(PreparedExecutionServiceRegistration {
            request,
            pins,
            bundle,
            material,
        })
    };
    prepare().map_err(|error| error.to_string())
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen)]
impl PreparedExecutionServiceRegistration {
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
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen(js_name = finishExecutionServiceRegistration))]
pub fn finish_execution_service_registration(
    prepared: &PreparedExecutionServiceRegistration,
    committed_json: &str,
    now_ms: f64,
) -> crate::Result<VerifiedExecutionServiceRegistration> {
    let finish = || -> Result<_> {
        let now = bootstrap::timestamp(now_ms)?;
        let committed: Committed<SpaceDocument> = parse(committed_json)?;
        let accepted = committed.commit.payload.accepted_at_ms;
        require(
            accepted <= now,
            "execution registration receipt is from the future",
        )?;
        let mut state = replay_policies(&prepared.pins, &prepared.bundle.journal)?;
        let verified = verify_execution_service_registration(
            &state,
            &prepared.request,
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
            .minimum_head = Some(head);
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
        let material = RecoveryMaterial {
            format: prepared.material.format.clone(),
            version: prepared.material.version,
            root_secret_hex: prepared.material.root_secret_hex.clone(),
            recovery_secret_hex: prepared.material.recovery_secret_hex.clone(),
            scope: scope.clone(),
            pins: owner_pins,
            bundle: owner_bundle,
            entry: prepared.material.entry.clone(),
        };
        let material_json =
            zeroize::Zeroizing::new(json(&material).map_err(|_| {
                AuthorizationError::InvalidProof("private material encoding failed")
            })?);
        bootstrap::recovery::inspect_account_recovery_material(&material_json, now_ms).map_err(
            |_| AuthorizationError::InvalidProof("updated execution owner material is invalid"),
        )?;
        Ok(VerifiedExecutionServiceRegistration {
            resource: intent.payload.execution_resource.clone(),
            pins: personal_pins,
            bundle,
            scope,
            material_json,
        })
    };
    finish().map_err(|error| error.to_string())
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen)]
impl VerifiedExecutionServiceRegistration {
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
