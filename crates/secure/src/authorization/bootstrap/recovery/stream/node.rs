use super::*;
use crate::authorization::execution_handoff::ExecutionProofUsage;

#[derive(Debug)]
pub struct VerifiedStreamedRecoveryCommit {
    authorization: VerifiedClientAuthorization,
    input_hash: ObjectHash,
    committed_heads: Vec<PolicyStamp>,
    proof_usage: RecoveryProofCounts,
    proof: RecoveryProofUsage,
    delta: Vec<JournalEntry>,
    expected_heads: Vec<PolicyStamp>,
}

impl VerifiedStreamedRecoveryCommit {
    pub fn authorization(&self) -> &VerifiedClientAuthorization {
        &self.authorization
    }
    pub fn input_hash(&self) -> &ObjectHash {
        &self.input_hash
    }
    pub fn committed_heads(&self) -> &[PolicyStamp] {
        &self.committed_heads
    }
    pub fn proof_usage(&self) -> &RecoveryProofCounts {
        &self.proof_usage
    }
    pub fn record_proof_usage_into(&self, usage: &mut RecoveryProofUsage) {
        usage.merge(&self.proof);
    }
    pub fn expected_heads(&self) -> &[PolicyStamp] {
        &self.expected_heads
    }
    pub fn into_store_parts(self) -> (VerifiedPolicySet, Vec<PolicyPin>, Vec<JournalEntry>) {
        (
            self.authorization.policy_snapshot,
            self.authorization.policy_pins,
            self.delta,
        )
    }
}

fn heads(read: &RecoveryReadRequest, state: &VerifiedPolicySet) -> Result<Vec<PolicyStamp>> {
    [
        PolicyDomain::Account(read.account().clone()),
        PolicyDomain::Space(read.space().clone()),
    ]
    .into_iter()
    .map(|domain| {
        state
            .head(&domain)
            .cloned()
            .ok_or(AuthorizationError::PolicyConflict)
    })
    .collect()
}

fn ordinary_read(read: &RecoveryReadRequest) -> Result<&Signed<RecoveryContextRequest>> {
    match read {
        RecoveryReadRequest::Account(value) => Ok(value),
        RecoveryReadRequest::CrossNode(_) => Err(AuthorizationError::InvalidProof(
            "ordinary recovery cannot consume a cross-node read",
        )),
    }
}

fn verify_proposal(
    approved: &StreamedRecoveryNodeRequest,
    read: &RecoveryReadRequest,
    before: &VerifiedPolicySet,
    now: u64,
) -> Result<VerifiedPolicySet> {
    let original = ordinary_read(read)?;
    let node = read.source_node_public_key();
    read.verify(before, node, now)?;
    before.verify_service_device(read.space(), &DeviceId::from_key(node), now)?;
    let request = &approved.request;
    let current = heads(read, before)?;
    require(
        approved.context.request_hash == read.request_hash()?
            && current
                == [
                    request.expected_account_head.clone(),
                    request.expected_space_head.clone(),
                ]
            && request.account_recovery.payload.account == *read.account()
            && request.account_recovery.issuer == original.issuer,
        "recovery proposal changed its original read, recovery authority or exact heads",
    )?;
    validate_claim(&request.claim, node, read.nonce(), now)?;
    let previous = before
        .account(read.account())
        .ok_or(AuthorizationError::Revoked)?;
    let proposed = &request.account_recovery.payload;
    let root = &proposed.current_root;
    let client = read.new_device_public_key();
    require_independent_new_recovery_keys(before, root, client)?;
    require(
        BTreeSet::from([
            root.clone(),
            client.clone(),
            node.clone(),
            original.issuer.clone(),
        ])
        .len()
            == 4
            && !before
                .roots
                .values()
                .any(|key| key == root || key == client)
            && !before.devices.values().any(|value| {
                &value.payload.public_key == root || &value.payload.public_key == client
            })
            && !before
                .accounts
                .values()
                .flat_map(|account| &account.history)
                .any(|value| {
                    value.payload.recovery.keys.contains(root)
                        || value.payload.recovery.keys.contains(client)
                }),
        "recovery requires independent unused root and client keys",
    )?;
    let mut expected = previous.clone();
    expected.previous = Some(request.expected_account_head.clone());
    expected.authority_epoch = previous
        .authority_epoch
        .checked_add(1)
        .ok_or(AuthorizationError::PolicyConflict)?;
    expected.sequence = 0;
    expected.current_root = root.clone();
    expected.root_generation = previous
        .root_generation
        .checked_add(1)
        .ok_or(AuthorizationError::PolicyConflict)?;
    expected.authority = AuthorityBinding::from_key(node.clone());
    expected.devices = revoke_devices(
        before,
        &previous.devices,
        &mut expected.revoked_devices,
        Some(&ServiceId::from_key(node)),
    )?;
    expected.enrollment.clear();
    expected.authenticators.clear();
    expected.authenticator_approval = None;
    expected.device_enrollment = None;
    expected.root_acceptance = proposed.root_acceptance.clone();
    expected.recovery_approvals = proposed.recovery_approvals.clone();
    require(
        proposed == &expected,
        "recovery proposal changed policy outside its approved transition",
    )?;
    let mut after = before.clone();
    let head = after.install_account(request.account_recovery.clone())?;
    let certificate = &request.node_certificate;
    require(
        certificate.version == AUTH_VERSION
            && certificate.owner == PolicyDomain::Account(read.account().clone())
            && certificate.subject == Actor::Service(ServiceId::from_key(node))
            && certificate.public_key == *node
            && certificate.device == DeviceId::from_key(node)
            && certificate.uses == [DeviceUse::Noise, DeviceUse::ServiceInvocation]
            && certificate.possession.is_none()
            && certificate.enrollment_grant.is_none()
            && certificate.issued_under == head
            && certificate.nonce == *read.nonce()
            && certificate.not_before_ms >= request.claim.payload.issued_at_ms
            && certificate.not_before_ms < request.claim.payload.expires_at_ms,
        "recovery node certificate changed the approved personal service role",
    )?;
    validate_window(certificate.not_before_ms, certificate.expires_at_ms, now)?;
    Ok(after)
}

pub fn sign_recovery_node_possession_in_state(
    streamed: &StreamedRecoveryNodeRequest,
    read: &RecoveryReadRequest,
    snapshot: &Signed<RecoveryContextSnapshot>,
    witness: &Signed<RecoveryContextWitness>,
    before: &VerifiedPolicySet,
    node: &Identity,
    expected_nonce: &Nonce,
    now: u64,
) -> Result<Signature> {
    let _scope = crate::signature_verification::ScopedSignatureVerification::new();
    let mut usage = RecoveryProofUsage::new(canonical(streamed)?.len())?;
    let node_key = PublicKey::of(node);
    require(
        read.source_node_public_key() == &node_key && read.nonce() == expected_nonce,
        "recovery reached another source node or claim nonce",
    )?;
    let request_hash = read.request_hash()?;
    let reference = &streamed.context;
    let value = &snapshot.payload;
    let sealed = &witness.payload;
    require(
        reference.request_hash == request_hash
            && reference.snapshot_id == value.snapshot_id
            && reference.snapshot_hash == snapshot.hash()?
            && reference.witness_hash == witness.hash()?
            && value.version == AUTH_VERSION
            && sealed.version == AUTH_VERSION
            && value.kind == RecoveryContextKind::Account
            && sealed.kind == value.kind
            && value.request_hash == request_hash
            && sealed.request_hash == request_hash
            && value.saved_pins_hash == read.saved_pins_hash()?
            && snapshot.issuer == node_key
            && witness.issuer == node_key
            && value.source_node_public_key == node_key
            && sealed.source_node_public_key == node_key
            && value.claim == streamed.request.claim
            && sealed.claim_hash == value.claim.hash()?
            && sealed.snapshot_id == value.snapshot_id
            && value.target_heads == heads(read, before)?
            && sealed.target_heads == value.target_heads
            && sealed.page_count > 0
            && sealed.entry_count <= sealed.page_count
            && value.issued_at_ms >= read.issued_at_ms()
            && value.expires_at_ms <= read.expires_at_ms()
            && sealed.issued_at_ms >= value.issued_at_ms
            && sealed.expires_at_ms <= value.expires_at_ms,
        "recovery possession changed its exact completed context",
    )?;
    validate_window(value.issued_at_ms, value.expires_at_ms, now)?;
    validate_window(sealed.issued_at_ms, sealed.expires_at_ms, now)?;
    snapshot.verify_signature()?;
    witness.verify_signature()?;
    verify_proposal(streamed, read, before, now)?;
    usage.record_request(read)?;
    usage.record_snapshot(snapshot)?;
    usage.record_witness(witness)?;
    usage.record_current_source(read, before)?;
    usage.record_node_request(streamed, before)?;
    usage.finish()?;
    Signature::parse(
        &node
            .sign(
                "device-possession-v2",
                &streamed.request.node_certificate.possession_bytes()?,
            )
            .map_err(|_| AuthorizationError::InvalidProof("recovery node signing failed"))?,
    )
}

fn verify_delta_shape(
    input: &StreamedRecoveryCommitRequest,
    approved: &StreamedRecoveryNodeRequest,
    read: &RecoveryReadRequest,
    before: &VerifiedPolicySet,
) -> Result<()> {
    macro_rules! item {
        ($index:expr, $variant:ident) => {
            match input.journal_delta.get($index) {
                Some(JournalEntry::$variant(value)) => value,
                _ => {
                    return Err(AuthorizationError::InvalidProof(
                        "recovery changed the exact transition sequence",
                    ))
                }
            }
        };
    }
    let request = &approved.request;
    let root = &request.account_recovery.payload.current_root;
    let source = read.source_node_public_key();
    let client = item!(1, Device);
    let node = item!(2, Device);
    let mut node_template = request.node_certificate.clone();
    node_template.possession = node.payload.possession.clone();
    let mut client_template = request.node_certificate.clone();
    client_template.public_key = read.new_device_public_key().clone();
    client_template.device = DeviceId::from_key(read.new_device_public_key());
    client_template.subject = Actor::Account(read.account().clone());
    client_template.uses = vec![
        DeviceUse::Noise,
        DeviceUse::AccountInvocation,
        DeviceUse::EnrollDevice,
    ];
    client_template.nonce = client.payload.nonce.clone();
    client_template.possession = client.payload.possession.clone();
    require(
        client.payload == client_template
            && node.payload == node_template
            && client.payload.possession.is_some()
            && node.payload.possession.is_some()
            && client.issuer == *root
            && node.issuer == *root,
        "recovery changed its independent client or approved node certificate",
    )?;
    let account = item!(3, Account);
    let mut expected_account = request.account_recovery.payload.clone();
    expected_account.previous = Some(account_stamp(&request.account_recovery)?);
    expected_account.sequence = 1;
    expected_account.devices.retain(|hash| {
        before.devices.get(hash).is_some_and(|value| {
            value.payload.device != client.payload.device
                && value.payload.device != node.payload.device
        })
    });
    expected_account
        .devices
        .extend([client.hash()?, node.hash()?]);
    expected_account.devices.sort();
    expected_account.enrollment = vec![EnrollmentAuthority {
        device: client.payload.device.clone(),
        rights: vec![
            AccountRight::EnrollDevice,
            AccountRight::RevokeDevice,
            AccountRight::ManageAuthenticators,
        ],
        expires_at_ms: client.payload.expires_at_ms,
    }];
    expected_account.root_acceptance = None;
    expected_account.recovery_approvals.clear();
    require(
        account.payload == expected_account && account.issuer == *root,
        "recovery renewed unexpected account authority",
    )?;
    let recovery_space = item!(4, Space);
    let mut expected_space = before
        .space(read.space())
        .cloned()
        .ok_or(AuthorizationError::PolicyConflict)?;
    expected_space.previous = Some(request.expected_space_head.clone());
    expected_space.authority_epoch = expected_space
        .authority_epoch
        .checked_add(1)
        .ok_or(AuthorizationError::PolicyConflict)?;
    expected_space.sequence = 0;
    expected_space.current_root = root.clone();
    expected_space.root_generation = request.account_recovery.payload.root_generation;
    expected_space.authority = AuthorityBinding::from_key(source.clone());
    expected_space.recovery = request.account_recovery.payload.recovery.clone();
    expected_space.devices = revoke_devices(
        before,
        &expected_space.devices,
        &mut expected_space.revoked_devices,
        None,
    )?;
    expected_space.grants.clear();
    expected_space.memberships.clear();
    expected_space.governance_grant = None;
    expected_space.root_acceptance = None;
    expected_space.recovery_approvals.clear();
    require(
        recovery_space.payload == expected_space && recovery_space.issuer == *root,
        "recovery changed existing space data or memberships",
    )?;
    let base = if matches!(input.journal_delta.get(5), Some(JournalEntry::Resource(_))) {
        6
    } else {
        5
    };
    require(
        input.journal_delta.len() == base + 4,
        "recovery contains additional policy mutations",
    )?;
    let owner = item!(base, Grant);
    let owner_space = item!(base + 1, Space);
    let service = item!(base + 2, Grant);
    let final_space = item!(base + 3, Space);
    let audience = Audience {
        service: ServiceId::from_key(source),
        device: DeviceId::from_key(source),
    };
    let template = initial_owner_grant(
        read.account(),
        read.space(),
        &audience,
        &space_stamp(recovery_space)?,
        client.payload.not_before_ms,
        client.payload.expires_at_ms,
        owner.payload.nonce.clone(),
    );
    require(
        owner.payload == template && owner.issuer == *root && input.grant_leaf == owner.hash()?,
        "recovery changed its exact owner grant",
    )?;
    let ResourceScope::Exact(project) = &service.payload.scope else {
        return Err(AuthorizationError::InvalidProof(
            "recovered service must be project scoped",
        ));
    };
    let mut template = owner.payload.clone();
    template.scope = ResourceScope::Exact(project.clone());
    template.grantee = Grantee::Actor(Actor::Service(audience.service.clone()));
    template.actions = vec![
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
    template.parent = Some(owner.hash()?);
    template.remaining_delegations = 0;
    template.nonce = service.payload.nonce.clone();
    require(
        service.payload == template
            && service.issuer == *read.new_device_public_key()
            && project.space == *read.space(),
        "recovery changed its project service grant",
    )?;
    expected_space.previous = Some(space_stamp(recovery_space)?);
    expected_space.sequence = 1;
    expected_space.grants = vec![owner.hash()?];
    let has_project = before
        .space(read.space())
        .ok_or(AuthorizationError::PolicyConflict)?
        .resources
        .iter()
        .filter_map(|hash| before.resources.get(hash))
        .any(|value| {
            value.payload.kind == ResourceKind::Project && value.payload.host == audience.service
        });
    if base == 6 {
        let record = item!(5, Resource);
        require(
            !has_project
                && record.issuer == *root
                && record.payload.version == AUTH_VERSION
                && record.payload.space == *read.space()
                && record.payload.resource == project.resource
                && record.payload.kind == ResourceKind::Project
                && record.payload.host == audience.service
                && record.payload.issued_under == space_stamp(recovery_space)?
                && !before.resources.values().any(|value| {
                    value.payload.space == *read.space()
                        && value.payload.resource == project.resource
                }),
            "recovery moved an existing project or added an unrelated resource",
        )?;
        expected_space.resources.push(record.hash()?);
        expected_space.resources.sort();
    } else {
        require(
            has_project
                && before
                    .resource(&project.space, &project.resource)
                    .is_some_and(|value| {
                        value.payload.kind == ResourceKind::Project
                            && value.payload.host == audience.service
                    }),
            "recovery selected an unregistered source project",
        )?;
    }
    require(
        owner_space.payload == expected_space && owner_space.issuer == *root,
        "recovery changed resource registration",
    )?;
    expected_space.previous = Some(space_stamp(owner_space)?);
    expected_space.sequence = 2;
    expected_space.grants.push(service.hash()?);
    expected_space.grants.sort();
    require(
        final_space.payload == expected_space && final_space.issuer == *root,
        "recovery changed its final personal space",
    )
}

pub fn verify_recovery_stream_commit_in_state(
    input: &StreamedRecoveryCommitRequest,
    approved: &StreamedRecoveryNodeRequest,
    read: &RecoveryReadRequest,
    before: &VerifiedPolicySet,
    now: u64,
) -> Result<VerifiedStreamedRecoveryCommit> {
    verify_recovery_stream_commit(input, approved, read, before, None, None, now)
}

pub fn verify_recovery_stream_commit_with_source_proof(
    input: &StreamedRecoveryCommitRequest,
    approved: &StreamedRecoveryNodeRequest,
    read: &RecoveryReadRequest,
    before: &VerifiedPolicySet,
    source: &VerifiedRecoverySourceProof,
    now: u64,
) -> Result<VerifiedStreamedRecoveryCommit> {
    source.cache_for(read, before)?;
    verify_recovery_stream_commit(input, approved, read, before, Some(source), None, now)
}

pub fn verify_recovery_stream_commit_with_source_approval(
    input: &StreamedRecoveryCommitRequest,
    approved: &StreamedRecoveryNodeRequest,
    read: &RecoveryReadRequest,
    before: &VerifiedPolicySet,
    approval: &VerifiedRecoverySourceProposalApproval,
    now: u64,
) -> Result<VerifiedStreamedRecoveryCommit> {
    approval.validate(
        read,
        &approved.context,
        &ObjectHash::body(approved)?,
        before,
        now,
    )?;
    verify_recovery_stream_commit(input, approved, read, before, None, Some(approval), now)
}

fn verify_recovery_stream_commit(
    input: &StreamedRecoveryCommitRequest,
    approved: &StreamedRecoveryNodeRequest,
    read: &RecoveryReadRequest,
    before: &VerifiedPolicySet,
    source: Option<&VerifiedRecoverySourceProof>,
    approval: Option<&VerifiedRecoverySourceProposalApproval>,
    now: u64,
) -> Result<VerifiedStreamedRecoveryCommit> {
    let _scope = crate::signature_verification::ScopedSignatureVerification::new();
    let mut usage = RecoveryProofUsage::new(canonical(input)?.len())?;
    require(
        input.context == approved.context
            && input.context.request_hash == read.request_hash()?
            && input.request_hash == ObjectHash::body(&approved.request)?
            && input.expected_account_head == approved.request.expected_account_head
            && input.expected_space_head == approved.request.expected_space_head
            && input.new_device_public_key == *read.new_device_public_key()
            && input.claim_nonce == *read.nonce()
            && input.journal_delta.first()
                == Some(&JournalEntry::Account(
                    approved.request.account_recovery.clone(),
                ))
            && input.journal_delta.len() <= MAX_PROOF_OBJECTS,
        "recovery commit differs from its exact approved request",
    )?;
    if let Some(approval) = approval {
        require(
            approval.account() == &approved.request.account_recovery,
            "source approval changed its exact account proposal",
        )?;
        read.verify_approved_credentials(before, read.source_node_public_key(), now)?;
        before.verify_service_device(
            read.space(),
            &DeviceId::from_key(read.source_node_public_key()),
            now,
        )?;
        validate_claim(
            &approved.request.claim,
            read.source_node_public_key(),
            read.nonce(),
            now,
        )?;
    } else {
        verify_proposal(approved, read, before, now)?;
    }
    verify_delta_shape(input, approved, read, before)?;
    usage.record_request(read)?;
    usage.record_current_source(read, before)?;
    if let Some(approval) = approval {
        approval.record_proof_usage_into(&mut usage)?;
        usage.record_approved_node_request(approved)?;
        usage.record_approved_commit(input, before, approval)?;
    } else if let Some(source) = source {
        source.record_proof_usage_into(&mut usage);
        usage.record_node_request_with_source_proof(approved, before, source)?;
        usage.record_commit_with_source_proof(input, before, source)?;
    } else {
        usage.record_node_request(approved, before)?;
        usage.record_commit(input, before)?;
    }
    let mut after = before.clone();
    for (index, entry) in input.journal_delta.iter().enumerate() {
        if let Some(approval) = approval.filter(|_| index == 0) {
            approval::apply_locally_approved_account_recovery(
                &mut after,
                approval.account(),
                &input.expected_account_head,
            )?;
        } else {
            apply_policy_entry(&mut after, &[], entry.clone())?;
        }
    }
    let root = &approved.request.account_recovery.payload.current_root;
    let account = after
        .account(read.account())
        .ok_or(AuthorizationError::PolicyConflict)?;
    let space = after
        .space(read.space())
        .ok_or(AuthorizationError::PolicyConflict)?;
    require(
        account.current_root == *root
            && space.current_root == *root
            && account.authority
                == AuthorityBinding::from_key(read.source_node_public_key().clone())
            && space.authority == account.authority
            && account.recovery
                == before
                    .account(read.account())
                    .ok_or(AuthorizationError::PolicyConflict)?
                    .recovery,
        "recovery final roots, authority or recovery keys differ from its approval",
    )?;
    let committed_heads = heads(read, &after)?;
    let mut pins = read.saved_pins().to_vec();
    for (pin, head) in pins.iter_mut().zip(&committed_heads) {
        pin.minimum_head = Some(head.clone());
    }
    let bundle = ClientAuthorizationBundle {
        journal: input.journal_delta.clone(),
        grant_leaf: input.grant_leaf.clone(),
    };
    let authorization = verify_client_policy_state(
        after,
        &bundle,
        &pins,
        read.account().clone(),
        read.space().clone(),
        read.new_device_public_key().clone(),
        now,
    )?;
    require(
        authorization.audience_public_key() == read.source_node_public_key()
            && authorization.heads == committed_heads,
        "recovery authorization changed the approved source audience or heads",
    )?;
    let mut client_usage = ExecutionProofUsage::default();
    client_usage.record_budget(&authorization.budget)?;
    usage.merge_usage(&client_usage);
    let proof_usage = usage.clone().finish()?;
    Ok(VerifiedStreamedRecoveryCommit {
        authorization,
        input_hash: ObjectHash::body(input)?,
        committed_heads,
        proof_usage,
        proof: usage,
        delta: input.journal_delta.clone(),
        expected_heads: vec![
            input.expected_account_head.clone(),
            input.expected_space_head.clone(),
        ],
    })
}
