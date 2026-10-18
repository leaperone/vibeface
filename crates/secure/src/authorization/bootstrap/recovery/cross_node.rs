//! A live source authority commits recovery before another Node can install it.
use super::*;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CrossNodeRecoveryRequest {
    pub version: u16,
    pub account: AccountId,
    pub space: SpaceId,
    pub source_node_public_key: PublicKey,
    pub saved_pins: Vec<PolicyPin>,
    pub new_root_public_key: PublicKey,
    pub new_device_public_key: PublicKey,
    pub target_claim: Signed<NodeClaim>,
    pub source_read_resources: Vec<ResourceRef>,
    pub source_read_expires_at_ms: u64,
    pub nonce: Nonce,
    pub issued_at_ms: u64,
    pub expires_at_ms: u64,
}
impl sealed::Sealed for CrossNodeRecoveryRequest {}
impl SignedObject for CrossNodeRecoveryRequest {
    const DOMAIN: &'static str = "cross-node-recovery-request-v2";
}
fn bounded<T: Serialize>(value: &T) -> Result<()> {
    require(
        canonical(value)?.len() <= MAX_PROOF_BYTES,
        "cross-node recovery requires a bounded checkpoint",
    )
}
pub(super) fn verify_request_identity(
    state: &VerifiedPolicySet,
    signed: &Signed<CrossNodeRecoveryRequest>,
    now: u64,
) -> Result<()> {
    bounded(signed)?;
    signed.verify_signature()?;
    let r = &signed.payload;
    let account = state
        .account(&r.account)
        .ok_or(AuthorizationError::Revoked)?;
    let space = state.space(&r.space).ok_or(AuthorizationError::Revoked)?;
    require(
        r.version == AUTH_VERSION
            && r.space == SpaceId::personal(&r.account)
            && space.root
                == (SpaceRoot::Personal {
                    owner: r.account.clone(),
                    genesis_root: account.genesis_root.clone(),
                })
            && account.authority.public_key == r.source_node_public_key
            && space.authority.public_key == r.source_node_public_key
            && account.recovery.threshold == 1
            && account.recovery.keys == [signed.issuer.clone()]
            && r.target_claim.payload.node_public_key != r.source_node_public_key,
        "cross-node request changed current recovery key, source or personal space",
    )?;
    validate_window(r.issued_at_ms, r.expires_at_ms, now)?;
    require(
        r.expires_at_ms - r.issued_at_ms <= NODE_CLAIM_MAX_AGE_MS
            && r.expires_at_ms <= r.target_claim.payload.expires_at_ms
            && r.source_read_expires_at_ms > r.expires_at_ms,
        "cross-node request exceeds its claim or read lifetime",
    )?;
    validate_claim(
        &r.target_claim,
        &r.target_claim.payload.node_public_key,
        &r.target_claim.payload.claim_nonce,
        now,
    )?;
    let keys = BTreeSet::from([
        r.new_root_public_key.clone(),
        r.new_device_public_key.clone(),
        r.target_claim.payload.node_public_key.clone(),
        r.source_node_public_key.clone(),
        signed.issuer.clone(),
    ]);
    require(
        keys.len() == 5,
        "cross-node recovery keys must be independent and new",
    )?;
    require_independent_new_recovery_keys(state, &r.new_root_public_key, &r.new_device_public_key)?;
    require(
        !r.source_read_resources.is_empty()
            && r.source_read_resources.len() <= 32
            && r.source_read_resources
                .windows(2)
                .all(|p| p[0].resource < p[1].resource)
            && r.source_read_resources
                .iter()
                .all(|resource| resource.space == r.space),
        "cross-node recovery requires ordered exact personal project references",
    )
}
pub(super) fn verify_request(
    state: &VerifiedPolicySet,
    signed: &Signed<CrossNodeRecoveryRequest>,
    now: u64,
) -> Result<()> {
    verify_request_identity(state, signed, now)?;
    let request = &signed.payload;
    require(
        request.source_read_resources.iter().all(|resource| {
            state
                .resource(&resource.space, &resource.resource)
                .is_some_and(|record| {
                    record.payload.kind == ResourceKind::Project
                        && record.payload.host
                            == ServiceId::from_key(&request.source_node_public_key)
                })
        }),
        "cross-node recovery permits only exact existing source projects",
    )
}
pub fn verify_cross_node_recovery_request(
    state: &VerifiedPolicySet,
    request: &Signed<CrossNodeRecoveryRequest>,
    local_source: &PublicKey,
    now: u64,
) -> Result<()> {
    require(
        local_source == &request.payload.source_node_public_key,
        "wrong recovery source",
    )?;
    verify_request(state, request, now)
}
pub(super) fn source_read_actions() -> Vec<Action> {
    vec![
        Action::ConversationRead,
        Action::OperationStatusRead,
        Action::AttachmentRead,
    ]
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen(js_name = buildCrossNodeRecoveryRequest))]
#[allow(clippy::too_many_arguments)]
pub fn build_cross_node_recovery_request(
    material_json: &str,
    next_root: &Identity,
    next_client: &Identity,
    claim: &VerifiedNodeClaim,
    source_read_resources_json: &str,
    source_read_expires_at_ms: f64,
    now_ms: f64,
    expires_at_ms: f64,
) -> crate::Result<String> {
    let _scope = crate::signature_verification::ScopedSignatureVerification::new();
    let build = || -> Result<_> {
        let material = read_material(material_json)?;
        let recovery = validate_material(&material)?;
        let now = timestamp(now_ms)?;
        let resources: Vec<ResourceRef> = serde_json::from_str(source_read_resources_json)
            .map_err(|_| AuthorizationError::InvalidProof("invalid source read resources"))?;
        let request = Signed::sign(
            CrossNodeRecoveryRequest {
                version: AUTH_VERSION,
                account: material.scope.account_id.clone(),
                space: material.scope.space_id.clone(),
                source_node_public_key: material.entry.node_public_key.clone(),
                saved_pins: material.pins.clone(),
                new_root_public_key: PublicKey::of(next_root),
                new_device_public_key: PublicKey::of(next_client),
                target_claim: claim.claim.clone(),
                source_read_resources: resources,
                source_read_expires_at_ms: timestamp(source_read_expires_at_ms)?,
                nonce: Nonce::random()?,
                issued_at_ms: now,
                expires_at_ms: timestamp(expires_at_ms)?,
            },
            &recovery,
        )?;
        verify_request_identity(&material_state(&material)?, &request, now)?;
        Ok(request)
    };
    encode(&build().map_err(|e| e.to_string())?)
}
#[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen(js_name = inspectCrossNodeRecoverySources))]
pub fn inspect_cross_node_recovery_sources(
    material_json: &str,
    now_ms: f64,
) -> crate::Result<String> {
    let _scope = crate::signature_verification::ScopedSignatureVerification::new();
    timestamp(now_ms).map_err(|e| e.to_string())?;
    let material = read_material(material_json).map_err(|e| e.to_string())?;
    validate_material(&material).map_err(|e| e.to_string())?;
    let state = material_state(&material).map_err(|e| e.to_string())?;
    let space = state
        .space(&material.scope.space_id)
        .ok_or("missing recovery personal space")?;
    let mut source_projects: Vec<_> = space
        .resources
        .iter()
        .filter_map(|hash| state.resources.get(hash))
        .filter(|record| {
            record.payload.kind == ResourceKind::Project
                && record.payload.host == ServiceId::from_key(&material.entry.node_public_key)
        })
        .map(|record| ResourceRef {
            space: record.payload.space.clone(),
            resource: record.payload.resource.clone(),
        })
        .collect();
    source_projects.sort_by(|a, b| a.resource.cmp(&b.resource));
    encode(
        &serde_json::json!({"source_projects":source_projects,"entry":material.entry,"scope":material.scope}),
    )
}

pub(super) fn verify_possession_in_state(
    signed: &Signed<CrossNodeRecoveryRequest>,
    node_request: &RecoveryNodeRequest,
    source_certificate: &DeviceCertificate,
    before: &VerifiedPolicySet,
    now: u64,
) -> Result<()> {
    let r = &signed.payload;
    stream::RecoveryReadRequest::CrossNode(signed.clone()).verify(
        before,
        &r.source_node_public_key,
        now,
    )?;
    verify_request(before, signed, now)?;
    before.verify_service_device(
        &r.space,
        &DeviceId::from_key(&r.source_node_public_key),
        now,
    )?;
    let node = node_request;
    require(
        node.claim == r.target_claim
            && before.head(&PolicyDomain::Account(r.account.clone()))
                == Some(&node.expected_account_head)
            && before.head(&PolicyDomain::Space(r.space.clone()))
                == Some(&node.expected_space_head)
            && node.account_recovery.payload.account == r.account
            && node.account_recovery.payload.current_root == r.new_root_public_key
            && node.account_recovery.issuer == signed.issuer,
        "cross-node possession changed its original request or exact policy heads",
    )?;
    let mut account = before
        .account(&r.account)
        .cloned()
        .ok_or(AuthorizationError::PolicyConflict)?;
    account.previous = Some(node.expected_account_head.clone());
    account.authority_epoch = account
        .authority_epoch
        .checked_add(1)
        .ok_or(AuthorizationError::PolicyConflict)?;
    account.sequence = 0;
    account.current_root = r.new_root_public_key.clone();
    account.root_generation = account
        .root_generation
        .checked_add(1)
        .ok_or(AuthorizationError::PolicyConflict)?;
    account.authority = AuthorityBinding::from_key(r.target_claim.payload.node_public_key.clone());
    account.devices = revoke_devices(
        before,
        &account.devices,
        &mut account.revoked_devices,
        Some(&ServiceId::from_key(&r.source_node_public_key)),
    )?;
    account.enrollment.clear();
    account.authenticators.clear();
    account.authenticator_approval = None;
    account.device_enrollment = None;
    account.root_acceptance = node.account_recovery.payload.root_acceptance.clone();
    account.recovery_approvals = node.account_recovery.payload.recovery_approvals.clone();
    require(
        node.account_recovery.payload == account,
        "cross-node possession changed policy beyond its exact recovery intent",
    )?;
    let mut state = before.clone();
    let head = state.install_account(node.account_recovery.clone())?;
    let target_key = &r.target_claim.payload.node_public_key;
    let target = DeviceCertificate {
        version: AUTH_VERSION,
        owner: PolicyDomain::Account(r.account.clone()),
        subject: Actor::Service(ServiceId::from_key(target_key)),
        device: DeviceId::from_key(target_key),
        public_key: target_key.clone(),
        uses: vec![DeviceUse::Noise, DeviceUse::ServiceInvocation],
        possession: None,
        enrollment_grant: None,
        issued_under: head.clone(),
        nonce: r.target_claim.payload.claim_nonce.clone(),
        not_before_ms: node.node_certificate.not_before_ms,
        expires_at_ms: node.node_certificate.expires_at_ms,
    };
    let source = DeviceCertificate {
        version: AUTH_VERSION,
        owner: PolicyDomain::Account(r.account.clone()),
        subject: Actor::Service(ServiceId::from_key(&r.source_node_public_key)),
        device: DeviceId::from_key(&r.source_node_public_key),
        public_key: r.source_node_public_key.clone(),
        uses: vec![DeviceUse::Noise, DeviceUse::ServiceInvocation],
        possession: None,
        enrollment_grant: None,
        issued_under: head,
        nonce: r.nonce.clone(),
        not_before_ms: target.not_before_ms,
        expires_at_ms: r.source_read_expires_at_ms,
    };
    require(
        node.node_certificate == target
            && source_certificate == &source
            && target.not_before_ms >= r.target_claim.payload.issued_at_ms
            && target.not_before_ms < r.target_claim.payload.expires_at_ms
            && target.expires_at_ms >= source.expires_at_ms,
        "cross-node possession changed its target or source service role",
    )?;
    validate_window(target.not_before_ms, target.expires_at_ms, now)?;
    validate_window(source.not_before_ms, source.expires_at_ms, now)
}

pub(super) fn verify_delta_shape_in_state(
    r: &CrossNodeRecoveryRequest,
    node: &RecoveryNodeRequest,
    before: &VerifiedPolicySet,
    delta: &[JournalEntry],
) -> Result<()> {
    macro_rules! item {
        ($index:expr, $variant:ident) => {
            match delta.get($index) {
                Some(JournalEntry::$variant(value)) => value,
                _ => {
                    return Err(AuthorizationError::InvalidProof(
                        "recovery changed the exact transition sequence",
                    ))
                }
            }
        };
    }
    let client = item!(1, Device);
    let target = item!(2, Device);
    let mut target_template = node.node_certificate.clone();
    target_template.possession = target.payload.possession.clone();
    let mut client_template = node.node_certificate.clone();
    client_template.public_key = r.new_device_public_key.clone();
    client_template.device = DeviceId::from_key(&r.new_device_public_key);
    client_template.subject = Actor::Account(r.account.clone());
    client_template.uses = vec![
        DeviceUse::Noise,
        DeviceUse::AccountInvocation,
        DeviceUse::EnrollDevice,
    ];
    client_template.possession = client.payload.possession.clone();
    client_template.nonce = client.payload.nonce.clone();
    require(
        client.payload == client_template
            && target.payload == target_template
            && client.issuer == r.new_root_public_key
            && target.issuer == r.new_root_public_key,
        "recovery certificates exceed exact independent client and target roles",
    )?;
    let first_account = item!(3, Account);
    let mut expected = node.account_recovery.payload.clone();
    expected.previous = Some(account_stamp(&node.account_recovery)?);
    expected.sequence = 1;
    expected.devices.extend([client.hash()?, target.hash()?]);
    expected.devices.sort();
    expected.enrollment = vec![EnrollmentAuthority {
        device: client.payload.device.clone(),
        rights: vec![
            AccountRight::EnrollDevice,
            AccountRight::RevokeDevice,
            AccountRight::ManageAuthenticators,
        ],
        expires_at_ms: client.payload.expires_at_ms,
    }];
    expected.root_acceptance = None;
    expected.recovery_approvals.clear();
    require(
        first_account.payload == expected && first_account.issuer == r.new_root_public_key,
        "recovery renewed unexpected account management authority",
    )?;
    let space_recovery = item!(4, Space);
    let mut expected_space = before
        .space(&r.space)
        .cloned()
        .ok_or(AuthorizationError::PolicyConflict)?;
    expected_space.previous = Some(node.expected_space_head.clone());
    expected_space.authority_epoch = expected_space
        .authority_epoch
        .checked_add(1)
        .ok_or(AuthorizationError::PolicyConflict)?;
    expected_space.sequence = 0;
    expected_space.current_root = r.new_root_public_key.clone();
    expected_space.root_generation = node.account_recovery.payload.root_generation;
    expected_space.authority =
        AuthorityBinding::from_key(r.target_claim.payload.node_public_key.clone());
    expected_space.recovery = node.account_recovery.payload.recovery.clone();
    expected_space.devices = revoke_devices(
        before,
        &expected_space.devices,
        &mut expected_space.revoked_devices,
        Some(&ServiceId::from_key(&r.source_node_public_key)),
    )?;
    expected_space.grants.clear();
    expected_space.memberships.clear();
    expected_space.governance_grant = None;
    expected_space.root_acceptance = None;
    expected_space.recovery_approvals.clear();
    require(
        space_recovery.payload == expected_space && space_recovery.issuer == r.new_root_public_key,
        "recovery changed old space data, memberships or policy",
    )?;
    let base = if matches!(delta.get(5), Some(JournalEntry::Resource(_))) {
        6
    } else {
        5
    };
    let owner = item!(base, Grant);
    let owner_space = item!(base + 1, Space);
    let service = item!(base + 2, Grant);
    let service_space = item!(base + 3, Space);
    let source = item!(base + 4, Device);
    let final_account = item!(base + 5, Account);
    let source_space = item!(base + 6, Space);
    let final_space = item!(base + 7 + r.source_read_resources.len(), Space);
    require(
        delta.len() == base + 8 + r.source_read_resources.len(),
        "recovery contains additional policy steps",
    )?;
    let audience = Audience {
        service: ServiceId::from_key(&r.target_claim.payload.node_public_key),
        device: DeviceId::from_key(&r.target_claim.payload.node_public_key),
    };
    let owner_template = initial_owner_grant(
        &r.account,
        &r.space,
        &audience,
        &space_stamp(space_recovery)?,
        client.payload.not_before_ms,
        client.payload.expires_at_ms,
        owner.payload.nonce.clone(),
    );
    require(
        owner.payload == owner_template && owner.issuer == r.new_root_public_key,
        "recovery changed its target owner grant",
    )?;
    let ResourceScope::Exact(project) = &service.payload.scope else {
        return Err(AuthorizationError::InvalidProof(
            "recovery target service must be project scoped",
        ));
    };
    let mut service_template = owner.payload.clone();
    service_template.scope = ResourceScope::Exact(project.clone());
    service_template.grantee = Grantee::Actor(Actor::Service(audience.service.clone()));
    service_template.actions = vec![
        Action::ConversationRead,
        Action::TaskCreate,
        Action::TaskProgress,
        Action::ExecutionOpen,
        Action::ExecutionSend,
        Action::ExecutionCommand,
        Action::OperationStatusRead,
        Action::AttachmentRead,
        Action::AgentStart,
    ];
    service_template.parent = Some(owner.hash()?);
    service_template.remaining_delegations = 0;
    service_template.nonce = service.payload.nonce.clone();
    require(
        service.payload == service_template && service.issuer == r.new_device_public_key,
        "recovery renewed unexpected execution service grants",
    )?;
    expected_space = space_recovery.payload.clone();
    expected_space.previous = Some(space_stamp(space_recovery)?);
    expected_space.sequence = 1;
    expected_space.grants = vec![owner.hash()?];
    if base == 6 {
        let resource = item!(5, Resource);
        require(
            resource.issuer == r.new_root_public_key
                && resource.payload.version == AUTH_VERSION
                && resource.payload.space == r.space
                && resource.payload.resource == project.resource
                && resource.payload.kind == ResourceKind::Project
                && resource.payload.host == audience.service
                && resource.payload.issued_under == space_stamp(space_recovery)?
                && before.resource(&r.space, &project.resource).is_none(),
            "recovery silently rehosted an old project",
        )?;
        expected_space.resources.push(resource.hash()?);
        expected_space.resources.sort();
    } else {
        require(
            before
                .resource(&r.space, &project.resource)
                .is_some_and(|record| {
                    record.payload.kind == ResourceKind::Project
                        && record.payload.host == audience.service
                }),
            "recovery target project is not registered at its original host",
        )?;
    }
    require(
        project.space == r.space && owner_space.payload == expected_space,
        "recovery changed its target resource registration",
    )?;
    expected_space.previous = Some(space_stamp(owner_space)?);
    expected_space.sequence += 1;
    expected_space.grants.push(service.hash()?);
    expected_space.grants.sort();
    require(
        service_space.payload == expected_space,
        "recovery target service commit differs from exact grant",
    )?;
    expected = first_account.payload.clone();
    expected.previous = Some(account_stamp(first_account)?);
    expected.sequence += 1;
    expected.devices.retain(|hash| {
        before
            .devices
            .get(hash)
            .is_none_or(|d| d.payload.device != source.payload.device)
    });
    expected.devices.push(source.hash()?);
    expected.devices.sort();
    require(
        final_account.payload == expected && final_account.issuer == r.new_root_public_key,
        "recovery source service acquired account rights",
    )?;
    expected_space = service_space.payload.clone();
    expected_space.previous = Some(space_stamp(service_space)?);
    expected_space.sequence += 1;
    expected_space.devices.retain(|hash| {
        before
            .devices
            .get(hash)
            .is_none_or(|d| d.payload.device != source.payload.device)
    });
    expected_space.devices.push(source.hash()?);
    expected_space.devices.sort();
    require(
        source_space.payload == expected_space,
        "recovery source service registration changed space privileges",
    )?;
    expected_space.previous = Some(space_stamp(source_space)?);
    expected_space.sequence += 1;
    for index in 0..r.source_read_resources.len() {
        expected_space
            .grants
            .push(item!(base + 7 + index, Grant).hash()?);
    }
    expected_space.grants.sort();
    require(
        final_space.payload == expected_space,
        "recovery source reads changed unrelated policy",
    )
}

#[allow(clippy::too_many_arguments)]
pub(super) fn verify_delta_in_state(
    signed: &Signed<CrossNodeRecoveryRequest>,
    node_request: &RecoveryNodeRequest,
    source_certificate: &DeviceCertificate,
    pins: &[PolicyPin],
    delta: &[JournalEntry],
    grant_leaf: &ObjectHash,
    before: &VerifiedPolicySet,
    now: u64,
) -> Result<(VerifiedPolicySet, PolicyStamp, PolicyStamp)> {
    verify_possession_in_state(signed, node_request, source_certificate, before, now)?;
    verify_delta_from_state(
        signed,
        node_request,
        source_certificate,
        pins,
        delta,
        grant_leaf,
        before,
        now,
        before.clone(),
        false,
    )
}

#[allow(clippy::too_many_arguments)]
pub(super) fn verify_delta_after_locally_approved_account(
    signed: &Signed<CrossNodeRecoveryRequest>,
    node_request: &RecoveryNodeRequest,
    source_certificate: &DeviceCertificate,
    pins: &[PolicyPin],
    delta: &[JournalEntry],
    grant_leaf: &ObjectHash,
    before: &VerifiedPolicySet,
    now: u64,
    after_account: VerifiedPolicySet,
) -> Result<(VerifiedPolicySet, PolicyStamp, PolicyStamp)> {
    require(
        after_account.head(&PolicyDomain::Account(signed.payload.account.clone()))
            == Some(&account_stamp(&node_request.account_recovery)?)
            && after_account.account(&signed.payload.account)
                == Some(&node_request.account_recovery.payload),
        "cross-node approved account differs from its exact proposal",
    )?;
    verify_delta_from_state(
        signed,
        node_request,
        source_certificate,
        pins,
        delta,
        grant_leaf,
        before,
        now,
        after_account,
        true,
    )
}

#[allow(clippy::too_many_arguments)]
fn verify_delta_from_state(
    signed: &Signed<CrossNodeRecoveryRequest>,
    node_request: &RecoveryNodeRequest,
    source_certificate: &DeviceCertificate,
    pins: &[PolicyPin],
    delta: &[JournalEntry],
    grant_leaf: &ObjectHash,
    before: &VerifiedPolicySet,
    now: u64,
    mut after: VerifiedPolicySet,
    approved_account: bool,
) -> Result<(VerifiedPolicySet, PolicyStamp, PolicyStamp)> {
    bounded(&delta)?;
    let r = &signed.payload;
    require(
        pins.len() == r.saved_pins.len()
            && pins.iter().zip(&r.saved_pins).all(|(pin, saved)| {
                pin.domain == saved.domain && pin.genesis_root == saved.genesis_root
            }),
        "recovery changed pinned roots or original source history",
    )?;
    require(
        delta.first()
            == Some(&JournalEntry::Account(
                node_request.account_recovery.clone(),
            ))
            && delta.len() >= 13
            && delta.len() <= MAX_PROOF_OBJECTS
            && delta.iter().all(|entry| match entry {
                JournalEntry::Account(d) => d.payload.account == r.account,
                JournalEntry::Space(d) => d.payload.space == r.space,
                JournalEntry::Device(d) => {
                    d.payload.owner == PolicyDomain::Account(r.account.clone())
                        && [
                            r.new_device_public_key.clone(),
                            r.source_node_public_key.clone(),
                            r.target_claim.payload.node_public_key.clone(),
                        ]
                        .contains(&d.payload.public_key)
                }
                JournalEntry::Grant(g) => g.payload.owner_space == r.space,
                JournalEntry::Resource(record) => {
                    record.payload.space == r.space
                        && record.payload.host
                            == ServiceId::from_key(&r.target_claim.payload.node_public_key)
                }
                _ => false,
            }),
        "recovery delta contains unrelated policy mutations",
    )?;
    verify_delta_shape_in_state(r, node_request, before, delta)?;
    let owner_index = if matches!(delta.get(5), Some(JournalEntry::Resource(_))) {
        6
    } else {
        5
    };
    require(
        matches!(delta.get(owner_index), Some(JournalEntry::Grant(owner)) if owner.hash().as_ref() == Ok(grant_leaf)),
        "recovery changed its exact account owner grant",
    )?;
    for entry in delta.iter().skip(usize::from(approved_account)) {
        apply_policy_entry(&mut after, pins, entry.clone())?;
    }
    let account = after
        .account(&r.account)
        .ok_or(AuthorizationError::PolicyConflict)?;
    let space = after
        .space(&r.space)
        .ok_or(AuthorizationError::PolicyConflict)?;
    let source = after.find_device(&DeviceId::from_key(&r.source_node_public_key))?;
    let mut expected_source = source_certificate.clone();
    expected_source.possession = source.payload.possession.clone();
    require(
        source.payload == expected_source
            && source.issuer == r.new_root_public_key
            && account.current_root == r.new_root_public_key
            && space.current_root == r.new_root_public_key
            && account.authority
                == AuthorityBinding::from_key(r.target_claim.payload.node_public_key.clone())
            && space.authority == account.authority
            && account.recovery
                == before
                    .account(&r.account)
                    .ok_or(AuthorizationError::PolicyConflict)?
                    .recovery,
        "recovery final authority or source service differs from signed intent",
    )?;
    after.verify_transport_device(&DeviceId::from_key(&r.source_node_public_key), now)?;
    after.verify_transport_device(
        &DeviceId::from_key(&r.target_claim.payload.node_public_key),
        now,
    )?;
    after.verify_account_client_device(
        &r.account,
        &DeviceId::from_key(&r.new_device_public_key),
        now,
    )?;
    for resource in &r.source_read_resources {
        let original = before
            .resource(&resource.space, &resource.resource)
            .ok_or(AuthorizationError::PolicyConflict)?;
        require(
            after.resource(&resource.space, &resource.resource) == Some(original),
            "recovery moved an existing source project",
        )?;
        let matches: Vec<_> = space
            .grants
            .iter()
            .filter_map(|hash| after.grants.get(hash))
            .filter(|g| {
                g.payload.scope == ResourceScope::Exact(resource.clone())
                    && g.payload.audience.device == DeviceId::from_key(&r.source_node_public_key)
            })
            .collect();
        require(
            matches.len() == 1,
            "missing exact source project read grant",
        )?;
        let grant = matches[0];
        require(
            grant.issuer == r.new_root_public_key
                && grant.payload.grantee == Grantee::Actor(Actor::Account(r.account.clone()))
                && grant.payload.actions == source_read_actions()
                && grant.payload.remaining_delegations == 0
                && grant.payload.parent.is_none()
                && grant.payload.constraints == GrantConstraints::default()
                && grant.payload.audience
                    == (Audience {
                        service: ServiceId::from_key(&r.source_node_public_key),
                        device: DeviceId::from_key(&r.source_node_public_key),
                    })
                && grant.payload.expires_at_ms == r.source_read_expires_at_ms,
            "source recovery grant must be finite exact account read-only",
        )?;
    }
    require(
        space
            .grants
            .iter()
            .filter_map(|hash| after.grants.get(hash))
            .filter(|g| g.payload.audience.device == DeviceId::from_key(&r.source_node_public_key))
            .count()
            == r.source_read_resources.len(),
        "recovery silently renewed other source privileges",
    )?;
    let account_head = after
        .head(&PolicyDomain::Account(r.account.clone()))
        .cloned()
        .ok_or(AuthorizationError::PolicyConflict)?;
    let space_head = after
        .head(&PolicyDomain::Space(r.space.clone()))
        .cloned()
        .ok_or(AuthorizationError::PolicyConflict)?;
    require(
        pins.iter().all(|pin| {
            if pin.domain == account_head.domain {
                pin.minimum_head.as_ref() == Some(&account_head)
            } else if pin.domain == space_head.domain {
                pin.minimum_head.as_ref() == Some(&space_head)
            } else {
                r.saved_pins.iter().any(|saved| saved == pin)
            }
        }),
        "recovery after pins differ from verified final heads",
    )?;
    Ok((after, account_head, space_head))
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen)]
pub struct PreparedCrossNodeAccountRecovery {
    possession: stream::StreamedCrossNodeRecoveryPossessionRequest,
    prepared: PreparedAccountRecovery,
    client_secret_hex: String,
}
impl Drop for PreparedCrossNodeAccountRecovery {
    fn drop(&mut self) {
        self.client_secret_hex.zeroize();
    }
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen(js_name = prepareCrossNodeAccountRecovery))]
pub fn prepare_cross_node_account_recovery(
    material_json: &str,
    context: &stream::VerifiedRecoveryPolicyContext,
    next_root: &Identity,
    next_client: &Identity,
    certificate_expires_at_ms: f64,
    now_ms: f64,
) -> crate::Result<PreparedCrossNodeAccountRecovery> {
    let _scope = crate::signature_verification::ScopedSignatureVerification::new();
    let prepare = || -> Result<PreparedCrossNodeAccountRecovery> {
        let signed = match context.request() {
            stream::RecoveryReadRequest::CrossNode(value) => value,
            stream::RecoveryReadRequest::Account(_) => {
                return Err(AuthorizationError::InvalidProof(
                    "cross-node recovery requires its original cross-node context",
                ));
            }
        };
        let r = &signed.payload;
        let material = read_material(material_json)?;
        let recovery = validate_material(&material)?;
        let now = timestamp(now_ms)?;
        require(
            material.pins == r.saved_pins
                && material.scope.account_id == r.account
                && material.scope.space_id == r.space
                && material.entry.node_public_key == r.source_node_public_key
                && PublicKey::of(next_root) == r.new_root_public_key
                && PublicKey::of(next_client) == r.new_device_public_key
                && PublicKey::of(&recovery) == signed.issuer,
            "cross-node context differs from the saved private material or new keys",
        )?;
        validate_recovery_context_current(context, now)?;
        let mut prepared = prepare_account_recovery_in_state(
            &material,
            &recovery,
            context.verified_state().clone(),
            context.policy().pins(),
            context.policy().journal(),
            next_root,
            next_client,
            &r.target_claim,
            timestamp(certificate_expires_at_ms)?,
            now,
            Some(&ServiceId::from_key(&r.source_node_public_key)),
        )?;
        prepared.verified_context = Some(context.clone());
        let source_certificate = DeviceCertificate {
            version: AUTH_VERSION,
            owner: PolicyDomain::Account(r.account.clone()),
            subject: Actor::Service(ServiceId::from_key(&r.source_node_public_key)),
            device: DeviceId::from_key(&r.source_node_public_key),
            public_key: r.source_node_public_key.clone(),
            uses: vec![DeviceUse::Noise, DeviceUse::ServiceInvocation],
            possession: None,
            enrollment_grant: None,
            issued_under: account_stamp(&prepared.request.account_recovery)?,
            nonce: r.nonce.clone(),
            not_before_ms: now,
            expires_at_ms: r.source_read_expires_at_ms,
        };
        let possession = stream::StreamedCrossNodeRecoveryPossessionRequest {
            context: context.reference()?,
            request: signed.clone(),
            node_request: prepared.request.clone(),
            source_certificate,
        };
        verify_possession_in_state(
            signed,
            &possession.node_request,
            &possession.source_certificate,
            &prepared.before_state,
            now,
        )?;
        bounded(&possession)?;
        Ok(PreparedCrossNodeAccountRecovery {
            possession,
            prepared,
            client_secret_hex: next_client.secret_hex(),
        })
    };
    prepare().map_err(|error| error.to_string())
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen)]
impl PreparedCrossNodeAccountRecovery {
    #[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen(js_name = possessionRequestJson))]
    pub fn possession_request_json(&self) -> crate::Result<String> {
        encode(&self.possession)
    }
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen)]
pub struct PreparedCrossNodeRecoveryCommit {
    request: stream::StreamedCrossNodeRecoveryCommitRequest,
    created: Option<CreatedAccount>,
    client_secret_hex: String,
    recovery_material: Option<String>,
    before_material: Option<String>,
    source_entry: Option<RecoveryEntry>,
}
impl Drop for PreparedCrossNodeRecoveryCommit {
    fn drop(&mut self) {
        self.client_secret_hex.zeroize();
        if let Some(material) = &mut self.recovery_material {
            material.zeroize();
        }
        if let Some(material) = &mut self.before_material {
            material.zeroize();
        }
    }
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen(js_name = finishCrossNodeAccountRecovery))]
pub fn finish_cross_node_account_recovery(
    prepared: &PreparedCrossNodeAccountRecovery,
    next_root: &Identity,
    target_possession: &str,
    source_possession: &str,
    now_ms: f64,
) -> crate::Result<PreparedCrossNodeRecoveryCommit> {
    let _scope = crate::signature_verification::ScopedSignatureVerification::new();
    let finish =
        || -> Result<PreparedCrossNodeRecoveryCommit> {
            let mut created =
                finish_account_recovery(&prepared.prepared, next_root, target_possession, now_ms)
                    .map_err(|_| {
                    AuthorizationError::InvalidProof("invalid cross-node target possession")
                })?;
            let now = timestamp(now_ms)?;
            let r = &prepared.possession.request.payload;
            let mut state = created.authorization.policy_snapshot.clone();
            let mut source = prepared.possession.source_certificate.clone();
            source.possession = Some(Signature::parse(source_possession)?);
            let source = Signed::sign(source, next_root)?;
            let source_hash = state.install_device(source.clone(), now)?;
            let mut account = state
                .account(&r.account)
                .cloned()
                .ok_or(AuthorizationError::PolicyConflict)?;
            account.previous = state
                .head(&PolicyDomain::Account(r.account.clone()))
                .cloned();
            account.sequence += 1;
            account.devices.retain(|hash| {
                state
                    .devices
                    .get(hash)
                    .is_some_and(|d| d.payload.device != source.payload.device)
            });
            account.devices.push(source_hash.clone());
            account.devices.sort();
            let account = Signed::sign(account, next_root)?;
            let account_head = state.install_account(account.clone())?;
            let mut space = state
                .space(&r.space)
                .cloned()
                .ok_or(AuthorizationError::PolicyConflict)?;
            space.previous = state.head(&PolicyDomain::Space(r.space.clone())).cloned();
            space.sequence += 1;
            space.devices.retain(|hash| {
                state
                    .devices
                    .get(hash)
                    .is_some_and(|d| d.payload.device != source.payload.device)
            });
            space.devices.push(source_hash);
            space.devices.sort();
            let space = Signed::sign(space, next_root)?;
            let space_head = state.install_space(space.clone())?;
            let mut grants = Vec::new();
            for resource in &r.source_read_resources {
                let grant = Signed::sign(
                    Grant {
                        execution_source: None,
                        version: AUTH_VERSION,
                        owner_space: r.space.clone(),
                        scope: ResourceScope::Exact(resource.clone()),
                        grantee: Grantee::Actor(Actor::Account(r.account.clone())),
                        actions: source_read_actions(),
                        audience: Audience {
                            service: ServiceId::from_key(&r.source_node_public_key),
                            device: DeviceId::from_key(&r.source_node_public_key),
                        },
                        constraints: GrantConstraints::default(),
                        parent: None,
                        remaining_delegations: 0,
                        issued_under: space_head.clone(),
                        not_before_ms: now,
                        expires_at_ms: r.source_read_expires_at_ms,
                        nonce: Nonce::random()?,
                    },
                    next_root,
                )?;
                state.install_grant(grant.clone(), now)?;
                grants.push(grant);
            }
            let mut final_space = space.payload.clone();
            final_space.previous = Some(space_head);
            final_space.sequence += 1;
            for grant in &grants {
                final_space.grants.push(grant.hash()?);
            }
            final_space.grants.sort();
            let final_space = Signed::sign(final_space, next_root)?;
            let final_head = state.install_space(final_space.clone())?;
            let mut bundle = created.bundle.clone();
            let mut delta = vec![
                JournalEntry::Device(source),
                JournalEntry::Account(account),
                JournalEntry::Space(space),
            ];
            delta.extend(grants.into_iter().map(JournalEntry::Grant));
            delta.push(JournalEntry::Space(final_space));
            bundle.journal.extend(delta.iter().cloned());
            let mut pins = created.pins.clone();
            for pin in &mut pins {
                if pin.domain == account_head.domain {
                    pin.minimum_head = Some(account_head.clone());
                } else if pin.domain == final_head.domain {
                    pin.minimum_head = Some(final_head.clone());
                }
            }
            let genuine = created
                .authorization
                .imported_snapshot
                .as_ref()
                .ok_or(AuthorizationError::InvalidProof(
                    "cross-node recovery lost its genuine context",
                ))?
                .apply_delta(&delta)?
                .with_independent_pins(&pins)?;
            let mut authorization = verify_client_policy_state(
                state,
                &bundle,
                &pins,
                r.account.clone(),
                r.space.clone(),
                r.new_device_public_key.clone(),
                now,
            )?;
            authorization.imported_snapshot = Some(genuine);
            created.pins = pins.clone();
            created.bundle = bundle.clone();
            created.authorization = authorization;
            let request = stream::StreamedCrossNodeRecoveryCommitRequest {
                possession: prepared.possession.clone(),
                pins,
                journal_delta: bundle.journal[prepared.prepared.journal.len()..].to_vec(),
                grant_leaf: bundle.grant_leaf,
            };
            let context = prepared.prepared.verified_context.as_ref().ok_or(
                AuthorizationError::InvalidProof("cross-node recovery lost its original read"),
            )?;
            stream::cross::verify_cross_node_recovery_stream_commit_locally_prepared(
                &request,
                &prepared.possession,
                context,
                now,
            )?;
            Ok(PreparedCrossNodeRecoveryCommit {
                request,
                created: Some(created),
                client_secret_hex: prepared.client_secret_hex.clone(),
                recovery_material: None,
                before_material: None,
                source_entry: None,
            })
        };
    finish().map_err(|error| error.to_string())
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen)]
impl PreparedCrossNodeRecoveryCommit {
    #[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen(js_name = requestJson))]
    pub fn request_json(&self) -> crate::Result<String> {
        bounded(&self.request).map_err(|error| error.to_string())?;
        encode(&self.request)
    }
    #[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen(js_name = clientIdentity))]
    pub fn client_identity(&self) -> crate::Result<Identity> {
        Identity::from_secret_hex(&self.client_secret_hex)
            .map_err(|_| "invalid pending recovery client".to_owned())
    }
    #[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen(js_name = resumeContextJson))]
    pub fn resume_context_json(&self) -> crate::Result<String> {
        let material = read_material(
            self.recovery_material
                .as_ref()
                .ok_or("pending recovery backup has not been prepared")?,
        )
        .map_err(|error| error.to_string())?;
        encode(&serde_json::json!({"source":self.source_entry.as_ref()
            .ok_or("missing recovery source entry")?,"target":material.entry,"scope":material.scope,
            "context":self.request.possession.context,"input_hash":ObjectHash::body(&self.request)
                .map_err(|error| error.to_string())?}))
    }
    #[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen(js_name = publicReviewJson))]
    pub fn public_review_json(&self) -> crate::Result<String> {
        let r = &self.request.possession.request.payload;
        let service_grant = self
            .request
            .journal_delta
            .iter()
            .find_map(|entry| match entry {
                JournalEntry::Grant(value)
                    if value.payload.grantee
                        == Grantee::Actor(Actor::Service(ServiceId::from_key(
                            &r.target_claim.payload.node_public_key,
                        ))) =>
                {
                    Some(value)
                }
                _ => None,
            })
            .ok_or("missing recovered service grant")?;
        let ResourceScope::Exact(project) = &service_grant.payload.scope else {
            return Err("recovered service grant is not an exact project".to_owned());
        };
        encode(
            &serde_json::json!({"scope":ClientScope {account_id:r.account.clone(),
            space_id:r.space.clone(), device_id:DeviceId::from_key(&r.new_device_public_key)},
            "personal_project":project,"service_grant":service_grant,
            "source_read_resources":r.source_read_resources,"source_read_actions":source_read_actions(),
            "source_read_expires_at_ms":r.source_read_expires_at_ms,
            "target_node_public_key":r.target_claim.payload.node_public_key,
            "source_node_public_key":r.source_node_public_key}),
        )
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PendingCrossNodeRecoveryMaterial {
    format: String,
    version: u16,
    before_material: RecoveryMaterial,
    material: RecoveryMaterial,
    client_secret_hex: String,
    commit_request: stream::StreamedCrossNodeRecoveryCommitRequest,
    input_hash: ObjectHash,
    source_entry: RecoveryEntry,
}
impl Drop for PendingCrossNodeRecoveryMaterial {
    fn drop(&mut self) {
        self.client_secret_hex.zeroize();
    }
}

fn verify_pending_material(pending: &PendingCrossNodeRecoveryMaterial, now: u64) -> Result<()> {
    let recovery = validate_material(&pending.before_material)?;
    validate_material(&pending.material)?;
    let after = material_state(&pending.material)?;
    let input = &pending.commit_request;
    let signed = &input.possession.request;
    let r = &signed.payload;
    let client = Identity::from_secret_hex(&pending.client_secret_hex)
        .map_err(|_| AuthorizationError::InvalidProof("invalid pending cross-node client"))?;
    let account_head = input
        .journal_delta
        .iter()
        .rev()
        .find_map(|entry| match entry {
            JournalEntry::Account(value) => Some(account_stamp(value)),
            _ => None,
        })
        .ok_or(AuthorizationError::PolicyConflict)??;
    let space_head = input
        .journal_delta
        .iter()
        .rev()
        .find_map(|entry| match entry {
            JournalEntry::Space(value) => Some(space_stamp(value)),
            _ => None,
        })
        .ok_or(AuthorizationError::PolicyConflict)??;
    require(
        pending.format == "vibeface-cross-node-recovery-stream-pending"
            && pending.version == AUTH_VERSION
            && pending.input_hash == ObjectHash::body(input)?
            && input.possession.context.request_hash == signed.hash()?
            && input.possession.node_request.claim == r.target_claim
            && input.possession.node_request.node_certificate.not_before_ms <= now
            && pending.before_material.pins == r.saved_pins
            && pending.before_material.scope.account_id == r.account
            && pending.before_material.scope.space_id == r.space
            && pending.before_material.entry == pending.source_entry
            && pending.source_entry.node_public_key == r.source_node_public_key
            && PublicKey::of(&recovery) == signed.issuer
            && pending.material.pins == input.pins
            && pending.material.bundle.grant_leaf == input.grant_leaf
            && pending.material.scope.account_id == r.account
            && pending.material.scope.space_id == r.space
            && pending.material.scope.device_id == DeviceId::from_key(&r.new_device_public_key)
            && pending.material.entry.node_public_key == r.target_claim.payload.node_public_key
            && PublicKey::of(&client) == r.new_device_public_key
            && after
                .account(&r.account)
                .is_some_and(|account| account.current_root == r.new_root_public_key)
            && after.head(&account_head.domain) == Some(&account_head)
            && after.head(&space_head.domain) == Some(&space_head)
            && input.pins.iter().all(|pin| {
                pin.minimum_head.as_ref()
                    == if pin.domain == account_head.domain {
                        Some(&account_head)
                    } else if pin.domain == space_head.domain {
                        Some(&space_head)
                    } else {
                        None
                    }
            }),
        "pending cross-node recovery changed its original operation, secrets or saved materials",
    )?;
    bounded(input)?;
    signed.verify_signature()
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen(js_name = restorePreparedCrossNodeRecoveryCommit))]
pub fn restore_prepared_cross_node_recovery_commit(
    pending_json: &str,
    now_ms: f64,
) -> crate::Result<PreparedCrossNodeRecoveryCommit> {
    let _scope = crate::signature_verification::ScopedSignatureVerification::new();
    let restore =
        || -> Result<PreparedCrossNodeRecoveryCommit> {
            require(
                pending_json.len() <= MAX_RECOVERY_JOURNAL_BYTES,
                "pending cross-node recovery too large",
            )?;
            let pending: PendingCrossNodeRecoveryMaterial = serde_json::from_str(pending_json)
                .map_err(|_| {
                    AuthorizationError::InvalidProof("invalid streamed cross-node pending material")
                })?;
            verify_pending_material(&pending, timestamp(now_ms)?)?;
            Ok(PreparedCrossNodeRecoveryCommit {
                request: pending.commit_request.clone(),
                created: None,
                client_secret_hex: pending.client_secret_hex.clone(),
                recovery_material: Some(serde_json::to_string(&pending.material).map_err(
                    |_| AuthorizationError::InvalidProof("invalid pending after material"),
                )?),
                before_material: Some(serde_json::to_string(&pending.before_material).map_err(
                    |_| AuthorizationError::InvalidProof("invalid pending before material"),
                )?),
                source_entry: Some(pending.source_entry.clone()),
            })
        };
    restore().map_err(|error| error.to_string())
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen)]
impl PreparedCrossNodeRecoveryCommit {
    #[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen(js_name = recoveryMaterialJson))]
    pub fn recovery_material_json(
        &mut self,
        next_root: &Identity,
        previous_material_json: &str,
        endpoint: &str,
        now_ms: f64,
    ) -> crate::Result<String> {
        let _scope = crate::signature_verification::ScopedSignatureVerification::new();
        let created = self
            .created
            .as_ref()
            .ok_or("restored recovery requires normal policy authentication")?;
        let next_material = build_recovered_account_recovery_material(
            created,
            next_root,
            previous_material_json,
            endpoint,
            now_ms,
        )?;
        let before = read_material(previous_material_json).map_err(|error| error.to_string())?;
        let material = read_material(&next_material).map_err(|error| error.to_string())?;
        let pending = PendingCrossNodeRecoveryMaterial {
            format: "vibeface-cross-node-recovery-stream-pending".to_owned(),
            version: AUTH_VERSION,
            source_entry: before.entry.clone(),
            before_material: before,
            material,
            client_secret_hex: self.client_secret_hex.clone(),
            commit_request: self.request.clone(),
            input_hash: ObjectHash::body(&self.request).map_err(|error| error.to_string())?,
        };
        verify_pending_material(
            &pending,
            timestamp(now_ms).map_err(|error| error.to_string())?,
        )
        .map_err(|error| error.to_string())?;
        let encoded = encode(&pending)?;
        require(
            encoded.len() <= MAX_RECOVERY_JOURNAL_BYTES,
            "pending cross-node recovery too large",
        )
        .map_err(|error| error.to_string())?;
        self.recovery_material = Some(next_material);
        self.before_material = Some(previous_material_json.to_owned());
        self.source_entry = Some(pending.source_entry.clone());
        Ok(encoded)
    }
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen)]
pub struct VerifiedCrossNodeRecoveryResult {
    committed: stream::StreamedCrossNodeRecoveryCommitted,
    created: Option<CreatedAccount>,
    recovery_material: Option<String>,
    now: u64,
}
impl Drop for VerifiedCrossNodeRecoveryResult {
    fn drop(&mut self) {
        if let Some(material) = &mut self.recovery_material {
            material.zeroize();
        }
    }
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen(js_name = finishCrossNodeRecoveryCommit))]
pub fn finish_cross_node_recovery_commit(
    prepared: &PreparedCrossNodeRecoveryCommit,
    receipt_json: &str,
    now_ms: f64,
) -> crate::Result<VerifiedCrossNodeRecoveryResult> {
    let _scope = crate::signature_verification::ScopedSignatureVerification::new();
    let finish = || -> Result<VerifiedCrossNodeRecoveryResult> {
        require(
            receipt_json.len() <= MAX_PROOF_BYTES,
            "cross-node recovery receipt too large",
        )?;
        let receipt: Signed<stream::StreamedCrossNodeRecoveryCommitReceipt> =
            serde_json::from_str(receipt_json).map_err(|_| {
                AuthorizationError::InvalidProof("invalid streamed cross-node receipt")
            })?;
        let committed = stream::StreamedCrossNodeRecoveryCommitted {
            request: prepared.request.clone(),
            receipt,
        };
        let mut usage = stream::RecoveryProofUsage::new(canonical(&committed)?.len())?;
        usage.record_cross_committed_carrier(&committed)?;
        let now = timestamp(now_ms)?;
        stream::cross::verify_receipt_binding(&committed, now)?;
        usage.finish()?;
        Ok(VerifiedCrossNodeRecoveryResult {
            committed,
            created: prepared.created.clone(),
            recovery_material: prepared.recovery_material.clone(),
            now,
        })
    };
    finish().map_err(|error| error.to_string())
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen)]
impl VerifiedCrossNodeRecoveryResult {
    #[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen(js_name = installationJson))]
    pub fn installation_json(&self) -> crate::Result<String> {
        bounded(&self.committed).map_err(|error| error.to_string())?;
        encode(&self.committed)
    }
    #[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen(js_name = pinsJson))]
    pub fn pins_json(&self) -> crate::Result<String> {
        encode(&self.committed.request.pins)
    }
    #[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen(js_name = scopeJson))]
    pub fn scope_json(&self) -> crate::Result<String> {
        let _scope = crate::signature_verification::ScopedSignatureVerification::new();
        let r = &self.committed.request.possession.request.payload;
        let scope = self
            .created
            .as_ref()
            .and_then(|created| {
                verify_client_policy_state(
                    created.authorization.policy_snapshot.clone(),
                    &created.bundle,
                    &created.pins,
                    r.account.clone(),
                    r.space.clone(),
                    r.new_device_public_key.clone(),
                    self.now,
                )
                .ok()
            })
            .map(|auth| auth.scope().clone());
        encode(&scope)
    }
    #[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen(js_name = authorization))]
    pub fn authorization(&self) -> crate::Result<VerifiedClientAuthorization> {
        let _scope = crate::signature_verification::ScopedSignatureVerification::new();
        let created = self
            .created
            .as_ref()
            .ok_or("restored recovery requires normal policy authentication")?;
        let r = &self.committed.request.possession.request.payload;
        let mut auth = verify_client_policy_state(
            created.authorization.policy_snapshot.clone(),
            &created.bundle,
            &created.pins,
            r.account.clone(),
            r.space.clone(),
            r.new_device_public_key.clone(),
            self.now,
        )
        .map_err(|error| error.to_string())?;
        auth.imported_snapshot = created.authorization.imported_snapshot.clone();
        Ok(auth)
    }
    #[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen(js_name = finalRecoveryMaterialJson))]
    pub fn final_recovery_material_json(
        &self,
        endpoint: &str,
        now_ms: f64,
    ) -> crate::Result<String> {
        let _scope = crate::signature_verification::ScopedSignatureVerification::new();
        timestamp(now_ms).map_err(|error| error.to_string())?;
        let mut material = read_material(
            self.recovery_material
                .as_ref()
                .ok_or("missing saved pending recovery material")?,
        )
        .map_err(|error| error.to_string())?;
        validate_material(&material).map_err(|error| error.to_string())?;
        require(
            endpoint_is_valid(endpoint),
            "invalid recovered node endpoint",
        )
        .map_err(|error| error.to_string())?;
        material.entry.endpoint = endpoint.to_owned();
        encode(&material)
    }
    #[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen(js_name = recoveryMaterialJson))]
    pub fn recovery_material_json(
        &self,
        next_root: &Identity,
        previous_material_json: &str,
        endpoint: &str,
        now_ms: f64,
    ) -> crate::Result<String> {
        let _scope = crate::signature_verification::ScopedSignatureVerification::new();
        let created = self
            .created
            .as_ref()
            .ok_or("restored recovery requires normal policy authentication")?;
        build_recovered_account_recovery_material(
            created,
            next_root,
            previous_material_json,
            endpoint,
            now_ms,
        )
    }
    #[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen(js_name = sourceReadSelectionsJson))]
    pub fn source_read_selections_json(&self) -> crate::Result<String> {
        let input = &self.committed.request;
        let r = &input.possession.request.payload;
        let mut selections = Vec::new();
        for resource in &r.source_read_resources {
            let grant = input
                .journal_delta
                .iter()
                .find_map(|entry| match entry {
                    JournalEntry::Grant(value)
                        if value.payload.scope == ResourceScope::Exact(resource.clone())
                            && value.payload.audience.device
                                == DeviceId::from_key(&r.source_node_public_key) =>
                    {
                        Some(value)
                    }
                    _ => None,
                })
                .ok_or("missing recovered source read grant")?;
            selections.push(serde_json::json!({"project":resource,
                "target_public_key":r.source_node_public_key,
                "grant_leaf":grant.hash().map_err(|error| error.to_string())?,
                "pins":input.pins,"scope":ClientScope { account_id:r.account.clone(),
                    space_id:r.space.clone(),device_id:DeviceId::from_key(&r.new_device_public_key)}}));
        }
        encode(&selections)
    }
}
