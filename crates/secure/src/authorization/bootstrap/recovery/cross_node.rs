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
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CrossNodeRecoveryWitness {
    pub version: u16,
    pub request_hash: ObjectHash,
    pub expected_account_head: PolicyStamp,
    pub expected_space_head: PolicyStamp,
    pub context_hash: ObjectHash,
    pub accepted_at_ms: u64,
    pub expires_at_ms: u64,
}
impl sealed::Sealed for CrossNodeRecoveryWitness {}
impl SignedObject for CrossNodeRecoveryWitness {
    const DOMAIN: &'static str = "cross-node-recovery-witness-v2";
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CrossNodeRecoveryContext {
    pub request: Signed<CrossNodeRecoveryRequest>,
    pub witness: Signed<CrossNodeRecoveryWitness>,
    pub bundle: ClientAuthorizationBundle,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CrossNodeRecoveryPossessionRequest {
    pub context: CrossNodeRecoveryContext,
    pub node_request: RecoveryNodeRequest,
    pub source_certificate: DeviceCertificate,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CrossNodeRecoveryCommitRequest {
    pub possession: CrossNodeRecoveryPossessionRequest,
    pub pins: Vec<PolicyPin>,
    pub bundle: ClientAuthorizationBundle,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CrossNodeRecoveryCommitReceipt {
    pub version: u16,
    pub request_hash: ObjectHash,
    pub input_hash: ObjectHash,
    pub expected_account_head: PolicyStamp,
    pub expected_space_head: PolicyStamp,
    pub committed_account_head: PolicyStamp,
    pub committed_space_head: PolicyStamp,
    pub target_node_public_key: PublicKey,
    pub new_device_public_key: PublicKey,
    pub accepted_at_ms: u64,
}
impl sealed::Sealed for CrossNodeRecoveryCommitReceipt {}
impl SignedObject for CrossNodeRecoveryCommitReceipt {
    const DOMAIN: &'static str = "cross-node-recovery-commit-receipt-v2";
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CrossNodeRecoveryCommitted {
    pub request: CrossNodeRecoveryCommitRequest,
    pub receipt: Signed<CrossNodeRecoveryCommitReceipt>,
}

fn bounded<T: Serialize>(value: &T) -> Result<()> {
    require(
        canonical(value)?.len() <= MAX_RECOVERY_JOURNAL_BYTES,
        "cross-node recovery too large",
    )
}
fn verify_request(
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
        keys.len() == 5
            && r.new_root_public_key != account.current_root
            && r.new_device_public_key != account.current_root
            && !state.devices.values().any(|c| {
                c.payload.public_key == r.new_root_public_key
                    || c.payload.public_key == r.new_device_public_key
            })
            && !state.accounts.values().flat_map(|a| &a.history).any(|d| {
                d.payload.current_root == r.new_root_public_key
                    || d.payload.current_root == r.new_device_public_key
            }),
        "cross-node recovery keys must be independent and new",
    )?;
    require(
        !r.source_read_resources.is_empty()
            && r.source_read_resources.len() <= 32
            && r.source_read_resources
                .windows(2)
                .all(|p| p[0].resource < p[1].resource)
            && r.source_read_resources.iter().all(|resource| {
                resource.space == r.space
                    && state
                        .resource(&resource.space, &resource.resource)
                        .is_some_and(|record| {
                            record.payload.kind == ResourceKind::Project
                                && record.payload.host
                                    == ServiceId::from_key(&r.source_node_public_key)
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
fn verify_context(context: &CrossNodeRecoveryContext, now: u64) -> Result<VerifiedPolicySet> {
    bounded(context)?;
    let state = replay_policies(&context.request.payload.saved_pins, &context.bundle.journal)?;
    let w = &context.witness.payload;
    require(
        w.accepted_at_ms <= now,
        "recovery witness is from the future",
    )?;
    verify_request(&state, &context.request, w.accepted_at_ms)?;
    context.witness.verify_signature()?;
    require(
        w.version == AUTH_VERSION
            && context.witness.issuer == context.request.payload.source_node_public_key
            && w.request_hash == ObjectHash::body(&context.request)?
            && w.context_hash == ObjectHash::body(&context.bundle)?
            && w.expires_at_ms == context.request.payload.expires_at_ms
            && state.head(&PolicyDomain::Account(
                context.request.payload.account.clone(),
            )) == Some(&w.expected_account_head)
            && state.head(&PolicyDomain::Space(context.request.payload.space.clone()))
                == Some(&w.expected_space_head),
        "recovery witness changed its signed source context",
    )?;
    Ok(state)
}
pub fn create_cross_node_recovery_witness(
    state: &VerifiedPolicySet,
    request: &Signed<CrossNodeRecoveryRequest>,
    bundle: &ClientAuthorizationBundle,
    source: &Identity,
    now: u64,
) -> Result<CrossNodeRecoveryContext> {
    verify_request(state, request, now)?;
    require(
        PublicKey::of(source) == request.payload.source_node_public_key,
        "wrong recovery source signer",
    )?;
    let before = replay_policies(&request.payload.saved_pins, &bundle.journal)?;
    let account_domain = PolicyDomain::Account(request.payload.account.clone());
    let space_domain = PolicyDomain::Space(request.payload.space.clone());
    require(
        before.head(&account_domain) == state.head(&account_domain)
            && before.head(&space_domain) == state.head(&space_domain),
        "source witness requires exact live policy heads",
    )?;
    let context = CrossNodeRecoveryContext {
        request: request.clone(),
        bundle: bundle.clone(),
        witness: Signed::sign(
            CrossNodeRecoveryWitness {
                version: AUTH_VERSION,
                request_hash: ObjectHash::body(request)?,
                context_hash: ObjectHash::body(bundle)?,
                expected_account_head: state
                    .head(&account_domain)
                    .cloned()
                    .ok_or(AuthorizationError::PolicyConflict)?,
                expected_space_head: state
                    .head(&space_domain)
                    .cloned()
                    .ok_or(AuthorizationError::PolicyConflict)?,
                accepted_at_ms: now,
                expires_at_ms: request.payload.expires_at_ms,
            },
            source,
        )?,
    };
    verify_context(&context, now)?;
    Ok(context)
}

#[derive(Clone, Debug)]
pub struct VerifiedCrossNodeRecoveryPossession {
    request: CrossNodeRecoveryPossessionRequest,
}
impl VerifiedCrossNodeRecoveryPossession {
    pub fn request_hash(&self) -> Result<ObjectHash> {
        ObjectHash::body(&self.request.context.request)
    }
    pub fn possession_hash(&self) -> Result<ObjectHash> {
        ObjectHash::body(&self.request)
    }
    pub fn new_device_public_key(&self) -> &PublicKey {
        &self.request.context.request.payload.new_device_public_key
    }
}
fn verify_possession(
    request: &CrossNodeRecoveryPossessionRequest,
    now: u64,
) -> Result<VerifiedPolicySet> {
    let mut state = verify_context(&request.context, now)?;
    let r = &request.context.request.payload;
    verify_request(&state, &request.context.request, now)?;
    let n = &request.node_request;
    let w = &request.context.witness.payload;
    require(
        n.claim == r.target_claim
            && n.expected_account_head == w.expected_account_head
            && n.expected_space_head == w.expected_space_head
            && n.account_recovery.payload.account == r.account
            && n.account_recovery.payload.current_root == r.new_root_public_key
            && n.account_recovery.issuer == request.context.request.issuer
            && n.account_recovery.payload.authority
                == AuthorityBinding::from_key(r.target_claim.payload.node_public_key.clone()),
        "recovery possession changed the witnessed keys or heads",
    )?;
    let original = state.clone();
    let mut expected_account = state
        .account(&r.account)
        .cloned()
        .ok_or(AuthorizationError::PolicyConflict)?;
    expected_account.previous = Some(w.expected_account_head.clone());
    expected_account.authority_epoch = expected_account
        .authority_epoch
        .checked_add(1)
        .ok_or(AuthorizationError::PolicyConflict)?;
    expected_account.sequence = 0;
    expected_account.current_root = r.new_root_public_key.clone();
    expected_account.root_generation = expected_account
        .root_generation
        .checked_add(1)
        .ok_or(AuthorizationError::PolicyConflict)?;
    expected_account.authority =
        AuthorityBinding::from_key(r.target_claim.payload.node_public_key.clone());
    expected_account.devices = revoke_devices(
        &state,
        &expected_account.devices,
        &mut expected_account.revoked_devices,
        Some(&ServiceId::from_key(&r.source_node_public_key)),
    )?;
    expected_account.enrollment.clear();
    expected_account.authenticators.clear();
    expected_account.authenticator_approval = None;
    expected_account.device_enrollment = None;
    expected_account.root_acceptance = n.account_recovery.payload.root_acceptance.clone();
    expected_account.recovery_approvals = n.account_recovery.payload.recovery_approvals.clone();
    require(
        n.account_recovery.payload == expected_account,
        "recovery altered account policy beyond its exact signed intent",
    )?;
    let head = state.install_account(n.account_recovery.clone())?;
    let expected = DeviceCertificate {
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
        not_before_ms: n.node_certificate.not_before_ms,
        expires_at_ms: r.source_read_expires_at_ms,
    };
    require(
        request.source_certificate == expected
            && n.node_certificate.expires_at_ms >= expected.expires_at_ms,
        "recovery source certificate exceeds its exact read-only intent",
    )?;
    Ok(original)
}
pub fn verify_cross_node_recovery_possession(
    request: &CrossNodeRecoveryPossessionRequest,
    local_target: &PublicKey,
    expected_claim_nonce: &Nonce,
    now: u64,
) -> Result<VerifiedCrossNodeRecoveryPossession> {
    verify_possession(request, now)?;
    let r = &request.context.request.payload;
    require(
        local_target == &r.target_claim.payload.node_public_key
            && expected_claim_nonce == &r.target_claim.payload.claim_nonce,
        "wrong recovery target or local nonce",
    )?;
    Ok(VerifiedCrossNodeRecoveryPossession {
        request: request.clone(),
    })
}
pub fn sign_cross_node_recovery_possession(
    request: &CrossNodeRecoveryPossessionRequest,
    node: &Identity,
    expected_claim_nonce: &Nonce,
    now: u64,
) -> Result<Signature> {
    verify_cross_node_recovery_possession(
        request,
        &PublicKey::of(node),
        expected_claim_nonce,
        now,
    )?;
    let signature = sign_recovery_node_possession(
        &encode(&request.node_request).map_err(|_| AuthorizationError::PolicyConflict)?,
        node,
        &encode(&request.context.request.payload.saved_pins)
            .map_err(|_| AuthorizationError::PolicyConflict)?,
        &encode(&request.context.bundle).map_err(|_| AuthorizationError::PolicyConflict)?,
        expected_claim_nonce.as_str(),
        now as f64,
    )
    .map_err(|_| AuthorizationError::InvalidProof("invalid cross-node recovery possession"))?;
    Signature::parse(&signature)
}
pub fn sign_cross_node_recovery_source_possession(
    state: &VerifiedPolicySet,
    request: &CrossNodeRecoveryPossessionRequest,
    source: &Identity,
    now: u64,
) -> Result<Signature> {
    verify_possession(request, now)?;
    verify_request(state, &request.context.request, now)?;
    let w = &request.context.witness.payload;
    require(
        PublicKey::of(source) == request.context.request.payload.source_node_public_key
            && state.head(&w.expected_account_head.domain) == Some(&w.expected_account_head)
            && state.head(&w.expected_space_head.domain) == Some(&w.expected_space_head),
        "source possession has stale policy heads",
    )?;
    Signature::parse(
        &source
            .sign(
                "device-possession-v2",
                &request.source_certificate.possession_bytes()?,
            )
            .map_err(|_| AuthorizationError::InvalidProof("source possession signing failed"))?,
    )
}

#[derive(Clone, Debug)]
pub struct VerifiedCrossNodeRecoveryCommit {
    request: CrossNodeRecoveryCommitRequest,
    committed_account_head: PolicyStamp,
    committed_space_head: PolicyStamp,
}
impl VerifiedCrossNodeRecoveryCommit {
    pub fn account(&self) -> &AccountId {
        &self.request.possession.context.request.payload.account
    }
    pub fn space(&self) -> &SpaceId {
        &self.request.possession.context.request.payload.space
    }
    pub fn new_device_public_key(&self) -> &PublicKey {
        &self
            .request
            .possession
            .context
            .request
            .payload
            .new_device_public_key
    }
    pub fn target_node_public_key(&self) -> &PublicKey {
        &self
            .request
            .possession
            .context
            .request
            .payload
            .target_claim
            .payload
            .node_public_key
    }
    pub fn claim_nonce(&self) -> &Nonce {
        &self
            .request
            .possession
            .context
            .request
            .payload
            .target_claim
            .payload
            .claim_nonce
    }
    pub fn request_hash(&self) -> Result<ObjectHash> {
        ObjectHash::body(&self.request.possession.context.request)
    }
    pub fn input_hash(&self) -> Result<ObjectHash> {
        ObjectHash::body(&self.request)
    }
    pub fn possession_hash(&self) -> Result<ObjectHash> {
        ObjectHash::body(&self.request.possession)
    }
    pub fn expected_account_head(&self) -> &PolicyStamp {
        &self
            .request
            .possession
            .context
            .witness
            .payload
            .expected_account_head
    }
    pub fn expected_space_head(&self) -> &PolicyStamp {
        &self
            .request
            .possession
            .context
            .witness
            .payload
            .expected_space_head
    }
    pub fn pins(&self) -> &[PolicyPin] {
        &self.request.pins
    }
    pub fn bundle(&self) -> &ClientAuthorizationBundle {
        &self.request.bundle
    }
    pub fn journal_delta(&self) -> &[JournalEntry] {
        &self.request.bundle.journal[self.request.possession.context.bundle.journal.len()..]
    }
}
fn verify_delta_shape(
    request: &CrossNodeRecoveryCommitRequest,
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
    let r = &request.possession.context.request.payload;
    let node = &request.possession.node_request;
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

fn verify_completed(
    request: &CrossNodeRecoveryCommitRequest,
    now: u64,
) -> Result<VerifiedCrossNodeRecoveryCommit> {
    bounded(request)?;
    let before = verify_possession(&request.possession, now)?;
    let r = &request.possession.context.request.payload;
    require(
        request
            .bundle
            .journal
            .starts_with(&request.possession.context.bundle.journal)
            && request.pins.len() == r.saved_pins.len()
            && request.pins.iter().zip(&r.saved_pins).all(|(pin, saved)| {
                pin.domain == saved.domain && pin.genesis_root == saved.genesis_root
            }),
        "recovery changed pinned roots or original source history",
    )?;
    let delta = &request.bundle.journal[request.possession.context.bundle.journal.len()..];
    require(
        delta.first()
            == Some(&JournalEntry::Account(
                request.possession.node_request.account_recovery.clone(),
            ))
            && delta.len() >= 13
            && delta.len() <= 46
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
    verify_delta_shape(request, &before, delta)?;
    let after = replay_policies(&request.pins, &request.bundle.journal)?;
    let account = after
        .account(&r.account)
        .ok_or(AuthorizationError::PolicyConflict)?;
    let space = after
        .space(&r.space)
        .ok_or(AuthorizationError::PolicyConflict)?;
    let source = after.find_device(&DeviceId::from_key(&r.source_node_public_key))?;
    let mut expected_source = request.possession.source_certificate.clone();
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
        request.pins.iter().all(|pin| {
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
    Ok(VerifiedCrossNodeRecoveryCommit {
        request: request.clone(),
        committed_account_head: account_head,
        committed_space_head: space_head,
    })
}
pub fn verify_cross_node_recovery_commit(
    state: &VerifiedPolicySet,
    request: &CrossNodeRecoveryCommitRequest,
    local_source: &PublicKey,
    now: u64,
) -> Result<VerifiedCrossNodeRecoveryCommit> {
    let verified = verify_completed(request, now)?;
    verify_request(state, &request.possession.context.request, now)?;
    require(
        local_source
            == &request
                .possession
                .context
                .request
                .payload
                .source_node_public_key
            && state.head(&verified.expected_account_head().domain)
                == Some(verified.expected_account_head())
            && state.head(&verified.expected_space_head().domain)
                == Some(verified.expected_space_head()),
        "source recovery commit requires both exact current heads",
    )?;
    Ok(verified)
}
pub fn accept_cross_node_recovery_commit(
    verified: &VerifiedCrossNodeRecoveryCommit,
    source: &Identity,
    now: u64,
) -> Result<CrossNodeRecoveryCommitted> {
    require(
        PublicKey::of(source)
            == verified
                .request
                .possession
                .context
                .request
                .payload
                .source_node_public_key,
        "wrong recovery commit source signer",
    )?;
    verify_completed(&verified.request, now)?;
    let receipt = Signed::sign(
        CrossNodeRecoveryCommitReceipt {
            version: AUTH_VERSION,
            request_hash: verified.request_hash()?,
            input_hash: verified.input_hash()?,
            expected_account_head: verified.expected_account_head().clone(),
            expected_space_head: verified.expected_space_head().clone(),
            committed_account_head: verified.committed_account_head.clone(),
            committed_space_head: verified.committed_space_head.clone(),
            target_node_public_key: verified.target_node_public_key().clone(),
            new_device_public_key: verified.new_device_public_key().clone(),
            accepted_at_ms: now,
        },
        source,
    )?;
    Ok(CrossNodeRecoveryCommitted {
        request: verified.request.clone(),
        receipt,
    })
}
fn verify_committed(
    committed: &CrossNodeRecoveryCommitted,
    now: u64,
) -> Result<VerifiedCrossNodeRecoveryCommit> {
    let receipt = &committed.receipt;
    receipt.verify_signature()?;
    require(
        receipt.payload.accepted_at_ms <= now,
        "recovery commit receipt is from the future",
    )?;
    let verified = verify_completed(&committed.request, receipt.payload.accepted_at_ms)?;
    let r = &receipt.payload;
    require(
        r.version == AUTH_VERSION
            && receipt.issuer
                == committed
                    .request
                    .possession
                    .context
                    .request
                    .payload
                    .source_node_public_key
            && r.request_hash == verified.request_hash()?
            && r.input_hash == verified.input_hash()?
            && &r.expected_account_head == verified.expected_account_head()
            && &r.expected_space_head == verified.expected_space_head()
            && r.committed_account_head == verified.committed_account_head
            && r.committed_space_head == verified.committed_space_head
            && &r.target_node_public_key == verified.target_node_public_key()
            && &r.new_device_public_key == verified.new_device_public_key(),
        "recovery source commit receipt does not bind this exact transition",
    )?;
    Ok(verified)
}
pub fn verify_cross_node_recovery_installation(
    committed: &CrossNodeRecoveryCommitted,
    local_target: &PublicKey,
    expected_claim_nonce: &Nonce,
    now: u64,
) -> Result<VerifiedCrossNodeRecoveryCommit> {
    let verified = verify_committed(committed, now)?;
    require(
        local_target == verified.target_node_public_key()
            && expected_claim_nonce == verified.claim_nonce(),
        "recovery installation changed local target claim",
    )?;
    let after = replay_policies(verified.pins(), &verified.bundle().journal)?;
    after.verify_transport_device(&DeviceId::from_key(local_target), now)?;
    Ok(verified)
}
fn source_read_actions() -> Vec<Action> {
    vec![
        Action::ConversationRead,
        Action::OperationStatusRead,
        Action::AttachmentRead,
    ]
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen)]
pub struct VerifiedCrossNodeRecoveryContext {
    context: CrossNodeRecoveryContext,
}
#[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen(js_name = buildCrossNodeRecoveryRequest))]
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
        verify_request(
            &replay_policies(&material.pins, &material.bundle.journal)?,
            &request,
            now,
        )?;
        Ok(request)
    };
    encode(&build().map_err(|e| e.to_string())?)
}
#[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen(js_name = verifyCrossNodeRecoveryContext))]
pub fn verify_cross_node_recovery_context(
    context_json: &str,
    original_request_json: &str,
    now_ms: f64,
) -> crate::Result<VerifiedCrossNodeRecoveryContext> {
    let verify = || -> Result<_> {
        require(
            context_json.len() <= MAX_RECOVERY_JOURNAL_BYTES,
            "recovery context too large",
        )?;
        let context: CrossNodeRecoveryContext = serde_json::from_str(context_json)
            .map_err(|_| AuthorizationError::InvalidProof("invalid cross-node context"))?;
        let expected: Signed<CrossNodeRecoveryRequest> =
            decode_signed(original_request_json.as_bytes())?;
        require(
            context.request == expected,
            "recovery context is for another original request",
        )?;
        let now = timestamp(now_ms)?;
        let state = verify_context(&context, now)?;
        verify_request(&state, &context.request, now)?;
        Ok(VerifiedCrossNodeRecoveryContext { context })
    };
    verify().map_err(|e| e.to_string())
}
#[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen)]
pub struct PreparedCrossNodeAccountRecovery {
    possession: CrossNodeRecoveryPossessionRequest,
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
    context: &VerifiedCrossNodeRecoveryContext,
    next_root: &Identity,
    next_client: &Identity,
    certificate_expires_at_ms: f64,
    now_ms: f64,
) -> crate::Result<PreparedCrossNodeAccountRecovery> {
    let r = &context.context.request.payload;
    let material = read_material(material_json).map_err(|e| e.to_string())?;
    validate_material(&material).map_err(|e| e.to_string())?;
    require(
        material.pins == r.saved_pins
            && material.scope.account_id == r.account
            && material.scope.space_id == r.space
            && material.entry.node_public_key == r.source_node_public_key
            && PublicKey::of(next_root) == r.new_root_public_key
            && PublicKey::of(next_client) == r.new_device_public_key
            && PublicKey::of(
                &Identity::from_secret_hex(&material.recovery_secret_hex)
                    .map_err(|_| "invalid recovery secret")?,
            ) == context.context.request.issuer,
        "cross-node prepared context differs from private saved recovery material",
    )
    .map_err(|e| e.to_string())?;
    let prepared = prepare_account_recovery_preserving_source(
        material_json,
        &encode(&context.context.bundle)?,
        next_root,
        next_client,
        &VerifiedNodeClaim {
            claim: r.target_claim.clone(),
        },
        certificate_expires_at_ms,
        now_ms,
        Some(&ServiceId::from_key(&r.source_node_public_key)),
    )?;
    let head = account_stamp(&prepared.request.account_recovery).map_err(|e| e.to_string())?;
    let source_certificate = DeviceCertificate {
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
        not_before_ms: timestamp(now_ms).map_err(|e| e.to_string())?,
        expires_at_ms: r.source_read_expires_at_ms,
    };
    let possession = CrossNodeRecoveryPossessionRequest {
        context: context.context.clone(),
        node_request: prepared.request.clone(),
        source_certificate,
    };
    verify_possession(&possession, timestamp(now_ms).map_err(|e| e.to_string())?)
        .map_err(|e| e.to_string())?;
    Ok(PreparedCrossNodeAccountRecovery {
        possession,
        prepared,
        client_secret_hex: next_client.secret_hex(),
    })
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
    request: CrossNodeRecoveryCommitRequest,
    created: CreatedAccount,
    client_secret_hex: String,
    recovery_material: Option<String>,
    source_entry: Option<RecoveryEntry>,
}
impl Drop for PreparedCrossNodeRecoveryCommit {
    fn drop(&mut self) {
        self.client_secret_hex.zeroize();
        if let Some(material) = &mut self.recovery_material {
            material.zeroize();
        }
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PendingCrossNodeRecoveryMaterial {
    format: String,
    version: u16,
    material: RecoveryMaterial,
    client_secret_hex: String,
    commit_request: CrossNodeRecoveryCommitRequest,
    source_entry: RecoveryEntry,
}
impl Drop for PendingCrossNodeRecoveryMaterial {
    fn drop(&mut self) {
        self.client_secret_hex.zeroize();
    }
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen(js_name = inspectCrossNodeRecoverySources))]
pub fn inspect_cross_node_recovery_sources(
    material_json: &str,
    now_ms: f64,
) -> crate::Result<String> {
    timestamp(now_ms).map_err(|e| e.to_string())?;
    let material = read_material(material_json).map_err(|e| e.to_string())?;
    validate_material(&material).map_err(|e| e.to_string())?;
    let state =
        replay_policies(&material.pins, &material.bundle.journal).map_err(|e| e.to_string())?;
    let space = state
        .space(&material.scope.space_id)
        .ok_or("missing recovery personal space")?;
    let source_projects: Vec<_> = space
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
    encode(
        &serde_json::json!({"source_projects":source_projects,"entry":material.entry,"scope":material.scope}),
    )
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen(js_name = restorePreparedCrossNodeRecoveryCommit))]
pub fn restore_prepared_cross_node_recovery_commit(
    pending_json: &str,
    now_ms: f64,
) -> crate::Result<PreparedCrossNodeRecoveryCommit> {
    require(
        pending_json.len() <= MAX_RECOVERY_JOURNAL_BYTES,
        "pending cross-node recovery too large",
    )
    .map_err(|e| e.to_string())?;
    let pending: PendingCrossNodeRecoveryMaterial =
        serde_json::from_str(pending_json).map_err(|_| "invalid pending cross-node recovery")?;
    let now = timestamp(now_ms).map_err(|e| e.to_string())?;
    let request = &pending.commit_request;
    let r = &request.possession.context.request.payload;
    let prepared_at = request.possession.source_certificate.not_before_ms;
    validate_material(&pending.material).map_err(|e| e.to_string())?;
    let client = Identity::from_secret_hex(&pending.client_secret_hex)
        .map_err(|_| "invalid pending recovery client")?;
    let root = Identity::from_secret_hex(&pending.material.root_secret_hex)
        .map_err(|_| "invalid pending recovery root")?;
    let recovery = Identity::from_secret_hex(&pending.material.recovery_secret_hex)
        .map_err(|_| "invalid pending recovery key")?;
    require(
        pending.format == "vibeface-cross-node-recovery-pending"
            && pending.version == AUTH_VERSION
            && prepared_at <= now
            && pending.material.pins == request.pins
            && pending.material.bundle == request.bundle
            && pending.material.scope.account_id == r.account
            && pending.material.scope.space_id == r.space
            && pending.material.entry.node_public_key == r.target_claim.payload.node_public_key
            && PublicKey::of(&root) == r.new_root_public_key
            && PublicKey::of(&client) == r.new_device_public_key
            && PublicKey::of(&recovery) == request.possession.context.request.issuer,
        "pending recovery secrets or history differ from original committed request",
    )
    .map_err(|e| e.to_string())?;
    verify_completed(request, prepared_at).map_err(|e| e.to_string())?;
    let authorization = verify_client_authorization(
        &encode(&request.bundle)?,
        &encode(&request.pins)?,
        r.account.as_str(),
        r.space.as_str(),
        r.new_device_public_key.as_str(),
        prepared_at as f64,
    )?;
    require(
        pending.source_entry.node_public_key == r.source_node_public_key
            && endpoint_is_valid(&pending.source_entry.endpoint),
        "pending recovery source entry differs from signed source pin",
    )
    .map_err(|e| e.to_string())?;
    Ok(PreparedCrossNodeRecoveryCommit {
        request: request.clone(),
        created: CreatedAccount {
            pins: request.pins.clone(),
            bundle: request.bundle.clone(),
            authorization,
        },
        client_secret_hex: pending.client_secret_hex.clone(),
        recovery_material: Some(encode(&pending.material)?),
        source_entry: Some(pending.source_entry.clone()),
    })
}
#[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen(js_name = finishCrossNodeAccountRecovery))]
pub fn finish_cross_node_account_recovery(
    prepared: &PreparedCrossNodeAccountRecovery,
    next_root: &Identity,
    target_possession: &str,
    source_possession: &str,
    now_ms: f64,
) -> crate::Result<PreparedCrossNodeRecoveryCommit> {
    let mut created =
        finish_account_recovery(&prepared.prepared, next_root, target_possession, now_ms)?;
    let now = timestamp(now_ms).map_err(|e| e.to_string())?;
    let extend = || -> Result<(Vec<PolicyPin>, ClientAuthorizationBundle)> {
        verify_possession(&prepared.possession, now)?;
        let r = &prepared.possession.context.request.payload;
        let mut state = replay_policies(&created.pins, &created.bundle.journal)?;
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
        bundle.journal.extend([
            JournalEntry::Device(source),
            JournalEntry::Account(account),
            JournalEntry::Space(space),
        ]);
        bundle
            .journal
            .extend(grants.into_iter().map(JournalEntry::Grant));
        bundle.journal.push(JournalEntry::Space(final_space));
        let mut pins = created.pins.clone();
        for pin in &mut pins {
            if pin.domain == account_head.domain {
                pin.minimum_head = Some(account_head.clone());
            } else if pin.domain == final_head.domain {
                pin.minimum_head = Some(final_head.clone());
            }
        }
        Ok((pins, bundle))
    };
    let (pins, bundle) = extend().map_err(|e| e.to_string())?;
    created.pins = pins.clone();
    created.bundle = bundle.clone();
    created.authorization = verify_client_authorization(
        &encode(&bundle)?,
        &encode(&pins)?,
        prepared.possession.context.request.payload.account.as_str(),
        prepared.possession.context.request.payload.space.as_str(),
        prepared
            .possession
            .context
            .request
            .payload
            .new_device_public_key
            .as_str(),
        now_ms,
    )?;
    let request = CrossNodeRecoveryCommitRequest {
        possession: prepared.possession.clone(),
        pins,
        bundle,
    };
    verify_completed(&request, now).map_err(|e| e.to_string())?;
    Ok(PreparedCrossNodeRecoveryCommit {
        request,
        created,
        client_secret_hex: prepared.client_secret_hex.clone(),
        recovery_material: None,
        source_entry: None,
    })
}
#[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen)]
impl PreparedCrossNodeRecoveryCommit {
    #[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen(js_name = requestJson))]
    pub fn request_json(&self) -> crate::Result<String> {
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
        .map_err(|e| e.to_string())?;
        encode(
            &serde_json::json!({"source":self.source_entry.as_ref().ok_or("missing recovery source entry")?,"target":material.entry,"scope":material.scope}),
        )
    }
    #[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen(js_name = publicReviewJson))]
    pub fn public_review_json(&self) -> crate::Result<String> {
        let r = &self.request.possession.context.request.payload;
        let state = replay_policies(&self.request.pins, &self.request.bundle.journal)
            .map_err(|e| e.to_string())?;
        let service_grant = state
            .space(&r.space)
            .ok_or("missing recovered space")?
            .grants
            .iter()
            .filter_map(|hash| state.grants.get(hash))
            .find(|grant| {
                grant.payload.grantee
                    == Grantee::Actor(Actor::Service(ServiceId::from_key(
                        &r.target_claim.payload.node_public_key,
                    )))
            })
            .ok_or("missing verified recovered service grant")?;
        let ResourceScope::Exact(project) = &service_grant.payload.scope else {
            return Err("recovered service grant is not an exact project".to_owned());
        };
        encode(
            &serde_json::json!({"scope":self.created.authorization.scope(),"personal_project":project,"service_grant":service_grant,
            "source_read_resources":r.source_read_resources,"source_read_actions":source_read_actions(),"source_read_expires_at_ms":r.source_read_expires_at_ms,
            "target_node_public_key":r.target_claim.payload.node_public_key,"source_node_public_key":r.source_node_public_key}),
        )
    }
    #[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen(js_name = recoveryMaterialJson))]
    pub fn recovery_material_json(
        &mut self,
        next_root: &Identity,
        previous_material_json: &str,
        endpoint: &str,
        now_ms: f64,
    ) -> crate::Result<String> {
        let next_material = build_recovered_account_recovery_material(
            &self.created,
            next_root,
            previous_material_json,
            endpoint,
            now_ms,
        )?;
        let material: RecoveryMaterial = serde_json::from_str(&next_material)
            .map_err(|_| "invalid prepared recovery material")?;
        let previous = read_material(previous_material_json).map_err(|e| e.to_string())?;
        validate_material(&previous).map_err(|e| e.to_string())?;
        self.recovery_material = Some(next_material);
        self.source_entry = Some(previous.entry.clone());
        encode(&PendingCrossNodeRecoveryMaterial {
            format: "vibeface-cross-node-recovery-pending".to_owned(),
            version: AUTH_VERSION,
            material,
            client_secret_hex: self.client_secret_hex.clone(),
            commit_request: self.request.clone(),
            source_entry: previous.entry.clone(),
        })
    }
}
#[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen)]
pub struct VerifiedCrossNodeRecoveryResult {
    committed: CrossNodeRecoveryCommitted,
    created: CreatedAccount,
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
    committed_json: &str,
    now_ms: f64,
) -> crate::Result<VerifiedCrossNodeRecoveryResult> {
    require(
        committed_json.len() <= MAX_RECOVERY_JOURNAL_BYTES,
        "recovery receipt too large",
    )
    .map_err(|e| e.to_string())?;
    let committed: CrossNodeRecoveryCommitted =
        serde_json::from_str(committed_json).map_err(|_| "invalid recovery receipt")?;
    require(
        committed.request == prepared.request,
        "recovery receipt belongs to another prepared transition",
    )
    .map_err(|e| e.to_string())?;
    verify_committed(&committed, timestamp(now_ms).map_err(|e| e.to_string())?)
        .map_err(|e| e.to_string())?;
    Ok(VerifiedCrossNodeRecoveryResult {
        committed,
        created: prepared.created.clone(),
        recovery_material: prepared.recovery_material.clone(),
        now: timestamp(now_ms).map_err(|e| e.to_string())?,
    })
}
#[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen)]
impl VerifiedCrossNodeRecoveryResult {
    #[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen(js_name = installationJson))]
    pub fn installation_json(&self) -> crate::Result<String> {
        encode(&self.committed)
    }
    #[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen(js_name = pinsJson))]
    pub fn pins_json(&self) -> crate::Result<String> {
        self.created.pins_json()
    }
    #[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen(js_name = bundleJson))]
    pub fn bundle_json(&self) -> crate::Result<String> {
        self.created.bundle_json()
    }
    #[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen(js_name = scopeJson))]
    pub fn scope_json(&self) -> crate::Result<String> {
        let r = &self.committed.request.possession.context.request.payload;
        let scope = verify_client_authorization(
            &self.created.bundle_json()?,
            &self.created.pins_json()?,
            r.account.as_str(),
            r.space.as_str(),
            r.new_device_public_key.as_str(),
            self.now as f64,
        )
        .ok()
        .map(|auth| auth.scope().clone());
        encode(&scope)
    }
    #[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen(js_name = finalRecoveryMaterialJson))]
    pub fn final_recovery_material_json(
        &self,
        endpoint: &str,
        now_ms: f64,
    ) -> crate::Result<String> {
        timestamp(now_ms).map_err(|e| e.to_string())?;
        let mut material = read_material(
            self.recovery_material
                .as_ref()
                .ok_or("missing saved pending recovery material")?,
        )
        .map_err(|e| e.to_string())?;
        validate_material(&material).map_err(|e| e.to_string())?;
        require(
            endpoint_is_valid(endpoint),
            "invalid recovered node endpoint",
        )
        .map_err(|e| e.to_string())?;
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
        build_recovered_account_recovery_material(
            &self.created,
            next_root,
            previous_material_json,
            endpoint,
            now_ms,
        )
    }
    #[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen(js_name = sourceReadSelectionsJson))]
    pub fn source_read_selections_json(&self) -> crate::Result<String> {
        let r = &self.committed.request.possession.context.request.payload;
        let state = replay_policies(&self.created.pins, &self.created.bundle.journal)
            .map_err(|e| e.to_string())?;
        let space = state.space(&r.space).ok_or("missing recovered space")?;
        let mut selections = Vec::new();
        for resource in &r.source_read_resources {
            let grant = space
                .grants
                .iter()
                .find(|hash| {
                    state.grants.get(*hash).is_some_and(|g| {
                        g.payload.scope == ResourceScope::Exact(resource.clone())
                            && g.payload.audience.device
                                == DeviceId::from_key(&r.source_node_public_key)
                    })
                })
                .ok_or("missing recovered source read grant")?;
            let mut bundle = self.created.bundle.clone();
            bundle.grant_leaf = grant.clone();
            let authorization = verify_client_authorization(
                &encode(&bundle)?,
                &self.created.pins_json()?,
                r.account.as_str(),
                r.space.as_str(),
                r.new_device_public_key.as_str(),
                self.now as f64,
            )?;
            selections.push(serde_json::json!({"project":resource,"target_public_key":r.source_node_public_key,"grant_leaf":grant,
                "pins":self.created.pins,"authorization":bundle,"scope":authorization.scope()}));
        }
        encode(&selections)
    }
}
