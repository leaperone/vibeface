//! Add one personal service Node without granting it business or account management rights.

use super::*;
use crate::authorization::{
    client_policy_import::require_confirmed_client_policy,
    enrollment_policy_import::{
        EnrollmentPolicyDescriptor, EnrollmentPolicyPurpose, EnrollmentPolicyReadRequest,
    },
    execution_handoff::ExecutionProofUsage,
    policy_import::VerifiedPolicySnapshot,
};

mod policy_import;
pub use policy_import::{
    AccountNodeEnrollmentImport, AccountNodeEnrollmentImportApproval,
    VerifiedAccountNodeEnrollmentContext, begin_account_node_enrollment_import,
    sign_account_node_enrollment_import_approval,
};

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct AccountNodeEnrollmentIntent {
    pub version: u16,
    pub account: AccountId,
    pub requesting_device: DeviceId,
    pub context_hash: ObjectHash,
    pub expected_head: PolicyStamp,
    pub authority: DeviceId,
    pub claim: Signed<NodeClaim>,
    pub certificate: DeviceCertificate,
    pub issued_at_ms: u64,
    pub expires_at_ms: u64,
}
impl sealed::Sealed for AccountNodeEnrollmentIntent {}
impl SignedObject for AccountNodeEnrollmentIntent {
    const DOMAIN: &'static str = "account-node-enrollment-v2";
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct AccountNodeEnrollmentCredential {
    pub intent: Signed<AccountNodeEnrollmentIntent>,
    pub descriptor: EnrollmentPolicyDescriptor,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct AccountNodeEnrollmentCommitRequest {
    pub intent: Signed<AccountNodeEnrollmentIntent>,
    pub descriptor: EnrollmentPolicyDescriptor,
    pub certificate: Signed<DeviceCertificate>,
    pub proposal: Signed<AccountDocument>,
}

#[derive(Clone, Debug)]
pub struct VerifiedAccountNodeEnrollmentProposal {
    request: AccountNodeEnrollmentCommitRequest,
    expected_head: PolicyStamp,
}
impl VerifiedAccountNodeEnrollmentProposal {
    pub fn request(&self) -> &AccountNodeEnrollmentCommitRequest {
        &self.request
    }
    pub fn expected_head(&self) -> &PolicyStamp {
        &self.expected_head
    }
    pub fn request_hash(&self) -> Result<ObjectHash> {
        ObjectHash::body(&self.request.intent)
    }
}

#[derive(Clone, Debug)]
#[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen)]
pub struct PreparedAccountNodeEnrollment {
    intent: Signed<AccountNodeEnrollmentIntent>,
    descriptor: EnrollmentPolicyDescriptor,
    read_request: Signed<EnrollmentPolicyReadRequest>,
    snapshot: VerifiedPolicySnapshot,
    authorization: VerifiedClientAuthorization,
}

#[derive(Clone, Debug)]
#[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen)]
pub struct PreparedAccountNodeEnrollmentCommit {
    request: AccountNodeEnrollmentCommitRequest,
    snapshot: VerifiedPolicySnapshot,
    authorization: VerifiedClientAuthorization,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AccountNodeInstallation {
    pub request: AccountNodeEnrollmentCommitRequest,
    pub committed: Committed<AccountDocument>,
}

#[derive(Clone, Debug)]
#[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen)]
pub struct VerifiedAccountNodeEnrollment {
    installation: AccountNodeInstallation,
    snapshot: VerifiedPolicySnapshot,
    authorization: Option<VerifiedClientAuthorization>,
}

fn root_material_snapshot(
    material_json: &str,
    snapshot: &VerifiedPolicySnapshot,
) -> Result<(AccountId, Identity, VerifiedPolicySnapshot)> {
    let material = read_material(material_json)?;
    validate_material(&material)?;
    let root = Identity::from_secret_hex(&material.root_secret_hex)
        .map_err(|_| AuthorizationError::InvalidProof("invalid node enrollment root"))?;
    let snapshot = snapshot.with_independent_pins(&material.pins)?;
    let account = snapshot
        .state()
        .account(&material.scope.account_id)
        .ok_or(AuthorizationError::Revoked)?;
    require(
        material.scope.space_id == SpaceId::personal(&material.scope.account_id)
            && account.current_root == PublicKey::of(&root)
            && account.authority.public_key == material.entry.node_public_key,
        "node enrollment changed current root or saved account authority",
    )?;
    Ok((material.scope.account_id.clone(), root, snapshot))
}

impl VerifiedAccountNodeEnrollment {
    pub fn pins(&self) -> &[PolicyPin] {
        self.snapshot.pins()
    }
    pub fn journal(&self) -> &[JournalEntry] {
        self.snapshot.journal()
    }
    pub fn account(&self) -> &AccountId {
        &self.installation.request.intent.payload.account
    }
    pub fn node_public_key(&self) -> &PublicKey {
        &self.installation.request.certificate.payload.public_key
    }
    pub fn claim_nonce(&self) -> &Nonce {
        &self
            .installation
            .request
            .intent
            .payload
            .claim
            .payload
            .claim_nonce
    }
    pub fn request_hash(&self) -> Result<ObjectHash> {
        ObjectHash::body(&self.installation.request.intent)
    }
}

fn fresh_node_key(state: &VerifiedPolicySet, key: &PublicKey) -> Result<()> {
    let device = DeviceId::from_key(key);
    require(
        !state
            .devices
            .values()
            .any(|cert| &cert.payload.public_key == key)
            && !state.roots.values().any(|root| root == key)
            && !state
                .accounts
                .values()
                .flat_map(|account| &account.history)
                .any(|doc| {
                    doc.payload.recovery.keys.contains(key)
                        || doc.payload.revoked_devices.contains(&device)
                }),
        "new node requires an independent, previously unused service key",
    )
}

fn verify_intent(
    state: &VerifiedPolicySet,
    signed: &Signed<AccountNodeEnrollmentIntent>,
    now: u64,
) -> Result<ExecutionProofUsage> {
    let i = &signed.payload;
    signed.verify_signature()?;
    let account = state
        .account(&i.account)
        .ok_or(AuthorizationError::Revoked)?;
    require(
        i.version == AUTH_VERSION
            && signed.issuer == account.current_root
            && state.head(&PolicyDomain::Account(i.account.clone())) == Some(&i.expected_head)
            && i.authority == account.authority.device,
        "node enrollment changed the current account root, head or authority",
    )?;
    validate_window(i.issued_at_ms, i.expires_at_ms, now)?;
    let manager = state.verify_account_client_device(&i.account, &i.requesting_device, now)?;
    require(
        manager.payload.uses.contains(&DeviceUse::EnrollDevice)
            && account.enrollment.iter().any(|authority| {
                authority.device == i.requesting_device
                    && now < authority.expires_at_ms
                    && authority.rights.contains(&AccountRight::EnrollDevice)
            }),
        "node enrollment requires a current account enrollment manager",
    )?;
    require(
        i.expires_at_ms - i.issued_at_ms <= NODE_CLAIM_MAX_AGE_MS
            && i.expires_at_ms <= i.claim.payload.expires_at_ms,
        "node enrollment exceeds its local claim lifetime",
    )?;
    let c = &i.certificate;
    validate_claim(&i.claim, &c.public_key, &c.nonce, now)?;
    require(
        c.version == AUTH_VERSION
            && c.owner == PolicyDomain::Account(i.account.clone())
            && c.subject == Actor::Service(ServiceId::from_key(&c.public_key))
            && c.device == DeviceId::from_key(&c.public_key)
            && c.uses == [DeviceUse::Noise, DeviceUse::ServiceInvocation]
            && c.possession.is_none()
            && c.enrollment_grant.is_none()
            && c.issued_under == i.expected_head
            && c.not_before_ms == i.issued_at_ms,
        "node enrollment changed the exact personal service certificate",
    )?;
    validate_window(c.not_before_ms, c.expires_at_ms, now)?;
    fresh_node_key(state, &c.public_key)?;
    let mut budget = Budget::default();
    state.validate_certificate(
        manager,
        now,
        &mut budget,
        &mut BTreeMap::new(),
        &mut BTreeSet::new(),
    )?;
    let mut usage = ExecutionProofUsage::default();
    usage.record_budget(&budget)?;
    usage.record_signed(signed)?;
    usage.record_signed(&i.claim)?;
    usage.enforce_limits()?;
    Ok(usage)
}

pub fn verify_account_node_enrollment_read(
    state: &VerifiedPolicySet,
    intent: &Signed<AccountNodeEnrollmentIntent>,
    descriptor: &EnrollmentPolicyDescriptor,
    source: &PublicKey,
    noise: &PublicKey,
    now: u64,
) -> Result<ExecutionProofUsage> {
    let _scope = crate::signature_verification::ScopedSignatureVerification::new();
    let mut usage = verify_intent(state, intent, now)?;
    let i = &intent.payload;
    let account = state
        .account(&i.account)
        .ok_or(AuthorizationError::Revoked)?;
    let domains = [
        PolicyDomain::Account(i.account.clone()),
        PolicyDomain::Space(SpaceId::personal(&i.account)),
    ];
    require(
        descriptor.version == AUTH_VERSION
            && descriptor.purpose == EnrollmentPolicyPurpose::AccountNode
            && descriptor.account == i.account
            && descriptor.requesting_device == i.requesting_device
            && descriptor.source_node_public_key == *source
            && account.authority.public_key == *source
            && DeviceId::from_key(noise) == i.requesting_device
            && descriptor.claim_hash.as_ref() == Some(&i.claim.hash()?)
            && descriptor.nonce == i.claim.payload.claim_nonce
            && descriptor.issued_at_ms == i.issued_at_ms
            && descriptor.expires_at_ms == i.expires_at_ms
            && descriptor.pins.len() == domains.len()
            && descriptor.target_heads.len() == domains.len()
            && descriptor
                .required_heads
                .windows(2)
                .all(|pair| pair[0].domain < pair[1].domain)
            && descriptor.pins.iter().zip(&domains).all(|(pin, domain)| {
                pin.domain == *domain
                    && pin.genesis_root == account.genesis_root
                    && pin
                        .minimum_head
                        .as_ref()
                        .is_none_or(|head| state.roots.contains_key(head))
            })
            && descriptor
                .target_heads
                .iter()
                .zip(&domains)
                .all(|(head, domain)| head.domain == *domain && state.head(domain) == Some(head))
            && descriptor
                .required_heads
                .iter()
                .all(|head| domains.contains(&head.domain) && state.roots.contains_key(head))
            && ObjectHash::body(descriptor)? == i.context_hash,
        "node enrollment changed its independent roots, exact heads, source, claim or Noise client",
    )?;
    let source_device = state.verify_transport_device(&DeviceId::from_key(source), now)?;
    let mut budget = Budget::default();
    state.validate_certificate(
        source_device,
        now,
        &mut budget,
        &mut BTreeMap::new(),
        &mut BTreeSet::new(),
    )?;
    usage.record_budget(&budget)?;
    usage.record_metadata(descriptor)?;
    usage.enforce_limits()?;
    Ok(usage)
}

fn proposed_account(
    state: &VerifiedPolicySet,
    intent: &AccountNodeEnrollmentIntent,
    hash: ObjectHash,
) -> Result<AccountDocument> {
    let mut document = state
        .account(&intent.account)
        .ok_or(AuthorizationError::Revoked)?
        .clone();
    document.previous = Some(intent.expected_head.clone());
    document.sequence = document
        .sequence
        .checked_add(1)
        .ok_or(AuthorizationError::PolicyConflict)?;
    document.devices.push(hash);
    document.devices.sort();
    document.authenticator_approval = None;
    document.device_enrollment = None;
    document.root_acceptance = None;
    document.recovery_approvals.clear();
    Ok(document)
}

pub fn verify_account_node_enrollment_proposal(
    state: &VerifiedPolicySet,
    request: &AccountNodeEnrollmentCommitRequest,
    now_ms: u64,
) -> Result<VerifiedAccountNodeEnrollmentProposal> {
    let _scope = crate::signature_verification::ScopedSignatureVerification::new();
    require(
        canonical(request)?.len() <= MAX_PROOF_BYTES,
        "node enrollment proposal too large",
    )?;
    let source = state
        .account(&request.intent.payload.account)
        .ok_or(AuthorizationError::Revoked)?
        .authority
        .public_key
        .clone();
    let requester = state
        .find_device(&request.intent.payload.requesting_device)?
        .payload
        .public_key
        .clone();
    verify_account_node_enrollment_read(
        state,
        &request.intent,
        &request.descriptor,
        &source,
        &requester,
        now_ms,
    )?;
    let mut certificate = request.intent.payload.certificate.clone();
    certificate.possession = request.certificate.payload.possession.clone();
    require(
        request.certificate.payload == certificate
            && request.certificate.issuer == request.intent.issuer
            && certificate.possession.is_some()
            && request.proposal.issuer == request.intent.issuer,
        "node enrollment changed the root approved certificate or issuer",
    )?;
    let mut after = state.clone();
    let hash = after.install_device(request.certificate.clone(), now_ms)?;
    require(
        request.proposal.payload == proposed_account(state, &request.intent.payload, hash)?,
        "node enrollment changed unrelated account policy",
    )?;
    after.install_account(request.proposal.clone())?;
    Ok(VerifiedAccountNodeEnrollmentProposal {
        request: request.clone(),
        expected_head: request.intent.payload.expected_head.clone(),
    })
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen(js_name = prepareAccountNodeEnrollment))]
pub fn prepare_account_node_enrollment(
    material_json: &str,
    authorization: &VerifiedClientAuthorization,
    requesting_client: &Identity,
    claim: &VerifiedNodeClaim,
    certificate_expires_at_ms: f64,
    now_ms: f64,
) -> crate::Result<PreparedAccountNodeEnrollment> {
    let _signature_scope = crate::signature_verification::ScopedSignatureVerification::new();
    let prepare = || -> Result<PreparedAccountNodeEnrollment> {
        let now = timestamp(now_ms)?;
        let (account, root, snapshot) = root_material_snapshot(
            material_json,
            require_confirmed_client_policy(authorization)?,
        )?;
        require(
            authorization.scope.account_id == account
                && authorization.scope.space_id == SpaceId::personal(&account)
                && authorization.scope.device_id
                    == DeviceId::from_key(&PublicKey::of(requesting_client)),
            "node enrollment requires this account's confirmed requesting client",
        )?;
        let state = snapshot.state();
        let source = state
            .account(&account)
            .ok_or(AuthorizationError::Revoked)?
            .authority
            .public_key
            .clone();
        let head = state
            .head(&PolicyDomain::Account(account.clone()))
            .ok_or(AuthorizationError::Revoked)?
            .clone();
        let key = claim.claim.payload.node_public_key.clone();
        let expires = claim
            .claim
            .payload
            .expires_at_ms
            .min(now.saturating_add(NODE_CLAIM_MAX_AGE_MS));
        let descriptor = EnrollmentPolicyDescriptor {
            version: AUTH_VERSION,
            purpose: EnrollmentPolicyPurpose::AccountNode,
            account: account.clone(),
            requesting_device: authorization.scope.device_id.clone(),
            source_node_public_key: source.clone(),
            pins: snapshot.pins().to_vec(),
            required_heads: snapshot
                .pins()
                .iter()
                .filter_map(|pin| pin.minimum_head.clone())
                .collect(),
            target_heads: snapshot.heads().to_vec(),
            claim_hash: Some(claim.claim.hash()?),
            nonce: claim.claim.payload.claim_nonce.clone(),
            issued_at_ms: now,
            expires_at_ms: expires,
        };
        let intent = Signed::sign(
            AccountNodeEnrollmentIntent {
                version: AUTH_VERSION,
                account: account.clone(),
                requesting_device: DeviceId::from_key(&PublicKey::of(requesting_client)),
                context_hash: ObjectHash::body(&descriptor)?,
                expected_head: head.clone(),
                authority: state
                    .account(&account)
                    .ok_or(AuthorizationError::Revoked)?
                    .authority
                    .device
                    .clone(),
                claim: claim.claim.clone(),
                certificate: DeviceCertificate {
                    version: AUTH_VERSION,
                    owner: PolicyDomain::Account(account),
                    subject: Actor::Service(ServiceId::from_key(&key)),
                    device: DeviceId::from_key(&key),
                    public_key: key,
                    uses: vec![DeviceUse::Noise, DeviceUse::ServiceInvocation],
                    possession: None,
                    enrollment_grant: None,
                    issued_under: head,
                    nonce: claim.claim.payload.claim_nonce.clone(),
                    not_before_ms: now,
                    expires_at_ms: timestamp(certificate_expires_at_ms)?,
                },
                issued_at_ms: now,
                expires_at_ms: expires,
            },
            &root,
        )?;
        verify_account_node_enrollment_read(
            state,
            &intent,
            &descriptor,
            &source,
            &PublicKey::of(requesting_client),
            now,
        )?;
        let credential = AccountNodeEnrollmentCredential {
            intent: intent.clone(),
            descriptor: descriptor.clone(),
        };
        let read_request = Signed::sign(
            EnrollmentPolicyReadRequest {
                version: AUTH_VERSION,
                purpose: EnrollmentPolicyPurpose::AccountNode,
                credential_hash: ObjectHash::body(&credential)?,
                descriptor_hash: ObjectHash::body(&descriptor)?,
                reader_public_key: PublicKey::of(requesting_client),
                target_public_key: claim.claim.payload.node_public_key.clone(),
                node_public_key: descriptor.source_node_public_key.clone(),
                nonce: Nonce::random()?,
                issued_at_ms: now,
                expires_at_ms: expires,
            },
            requesting_client,
        )?;
        Ok(PreparedAccountNodeEnrollment {
            intent,
            descriptor,
            read_request,
            snapshot,
            authorization: authorization.clone(),
        })
    };
    prepare().map_err(|e| e.to_string())
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen)]
impl PreparedAccountNodeEnrollment {
    #[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen(js_name = requestJson))]
    pub fn request_json(&self) -> crate::Result<String> {
        encode(&serde_json::json!({
            "credential":{"purpose":"account_node","credential":AccountNodeEnrollmentCredential {
                intent:self.intent.clone(),descriptor:self.descriptor.clone()
            }},
            "request":self.read_request,
        }))
    }
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen(js_name = signAccountNodeEnrollmentPossession))]
pub fn sign_account_node_enrollment_possession(
    context: &VerifiedAccountNodeEnrollmentContext,
    node: &Identity,
    expected_claim_nonce: &str,
    now_ms: f64,
) -> crate::Result<String> {
    let _scope = crate::signature_verification::ScopedSignatureVerification::new();
    let sign = || -> Result<String> {
        let request = &context.credential.intent;
        let now = timestamp(now_ms)?;
        verify_account_node_enrollment_read(
            context.policy.state(),
            request,
            &context.credential.descriptor,
            &context.credential.descriptor.source_node_public_key,
            &context.reader,
            now,
        )?;
        validate_claim(
            &request.payload.claim,
            &PublicKey::of(node),
            &Nonce::parse(expected_claim_nonce)?,
            now,
        )?;
        let mut certificate = request.payload.certificate.clone();
        certificate.prove_possession(node)?;
        Ok(certificate
            .possession
            .ok_or(AuthorizationError::InvalidProof("missing node possession"))?
            .to_string())
    };
    sign().map_err(|e| e.to_string())
}

fn decode_bounded<T: for<'de> Deserialize<'de>>(json: &str, limit: usize) -> Result<T> {
    require(json.len() <= limit, "node enrollment input too large")?;
    serde_json::from_str(json)
        .map_err(|_| AuthorizationError::InvalidProof("invalid node enrollment JSON"))
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen(js_name = prepareAccountNodeEnrollmentCommit))]
pub fn prepare_account_node_enrollment_commit(
    prepared: &PreparedAccountNodeEnrollment,
    material_json: &str,
    node_possession_signature: &str,
    now_ms: f64,
) -> crate::Result<PreparedAccountNodeEnrollmentCommit> {
    let _signature_scope = crate::signature_verification::ScopedSignatureVerification::new();
    let prepare = || -> Result<PreparedAccountNodeEnrollmentCommit> {
        let now = timestamp(now_ms)?;
        let (account, root, snapshot) = root_material_snapshot(material_json, &prepared.snapshot)?;
        let mut state = snapshot.state().clone();
        require(
            account == prepared.intent.payload.account
                && PublicKey::of(&root) == prepared.intent.issuer,
            "node enrollment material changed account or root",
        )?;
        verify_intent(&state, &prepared.intent, now)?;
        let mut certificate = prepared.intent.payload.certificate.clone();
        certificate.possession = Some(Signature::parse(node_possession_signature)?);
        let certificate = Signed::sign(certificate, &root)?;
        let hash = state.install_device(certificate.clone(), now)?;
        let proposal = Signed::sign(
            proposed_account(&state, &prepared.intent.payload, hash)?,
            &root,
        )?;
        let request = AccountNodeEnrollmentCommitRequest {
            intent: prepared.intent.clone(),
            descriptor: prepared.descriptor.clone(),
            certificate,
            proposal,
        };
        verify_account_node_enrollment_proposal(snapshot.state(), &request, now)?;
        Ok(PreparedAccountNodeEnrollmentCommit {
            request,
            snapshot,
            authorization: prepared.authorization.clone(),
        })
    };
    prepare().map_err(|e| e.to_string())
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen)]
impl PreparedAccountNodeEnrollmentCommit {
    #[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen(js_name = requestJson))]
    pub fn request_json(&self) -> crate::Result<String> {
        encode(&self.request)
    }
    #[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen(js_name = completionJournalJson))]
    pub fn completion_journal_json(&self) -> crate::Result<String> {
        encode(&[
            JournalEntry::Device(self.request.certificate.clone()),
            JournalEntry::Account(self.request.proposal.clone()),
        ])
    }
}

pub fn commit_account_node_enrollment_in_state(
    request: &AccountNodeEnrollmentCommitRequest,
    state: &VerifiedPolicySet,
    authority: &Identity,
    now: u64,
) -> Result<Vec<JournalEntry>> {
    let _signature_scope = crate::signature_verification::ScopedSignatureVerification::new();
    let mut state = state.clone();
    let verified = verify_account_node_enrollment_proposal(&state, request, now)?;
    let account = state
        .account(&request.intent.payload.account)
        .ok_or(AuthorizationError::Revoked)?;
    require(
        PublicKey::of(authority) == account.authority.public_key,
        "node enrollment commit requires the current local account authority",
    )?;
    let expires = request
        .intent
        .payload
        .expires_at_ms
        .min(now.saturating_add(EXECUTION_LEASE_MS));
    let previous = verified.expected_head.clone();
    let commit = Signed::sign(
        PolicyCommit {
            version: AUTH_VERSION,
            previous: previous.clone(),
            proposal_hash: request.proposal.hash()?,
            accepted_at_ms: now,
            issued_at_ms: now,
            expires_at_ms: expires,
            nonce: Nonce::random()?,
            dependencies: vec![previous.clone()],
        },
        authority,
    )?;
    let receipt = Signed::sign(
        FreshnessReceipt {
            version: AUTH_VERSION,
            current: previous,
            challenge_hash: commit.challenge_hash()?,
            issued_at_ms: now,
            expires_at_ms: expires,
        },
        authority,
    )?;
    let committed = Committed {
        proposal: request.proposal.clone(),
        commit,
        receipts: vec![receipt],
    };
    state.install_device(request.certificate.clone(), now)?;
    state.install_account_committed(committed.clone(), now)?;
    Ok(vec![
        JournalEntry::Device(request.certificate.clone()),
        JournalEntry::AccountCommit(committed),
    ])
}

fn finish(
    prepared: &PreparedAccountNodeEnrollmentCommit,
    delta: Vec<JournalEntry>,
    now: u64,
) -> Result<VerifiedAccountNodeEnrollment> {
    let [
        JournalEntry::Device(certificate),
        JournalEntry::AccountCommit(committed),
    ] = delta.as_slice()
    else {
        return Err(AuthorizationError::InvalidProof(
            "node enrollment requires its exact device and committed account delta",
        ));
    };
    require(
        certificate == &prepared.request.certificate
            && committed.proposal == prepared.request.proposal
            && committed.commit.payload.accepted_at_ms <= now,
        "node enrollment receipt changed the approved service or account proposal",
    )?;
    let accepted = committed.commit.payload.accepted_at_ms;
    verify_account_node_enrollment_proposal(
        prepared.snapshot.state(),
        &prepared.request,
        accepted,
    )?;
    let snapshot = prepared.snapshot.apply_delta(&delta)?;
    snapshot
        .state()
        .verify_transport_device(&certificate.payload.device, accepted)?;
    let mut pins = snapshot.pins().to_vec();
    for pin in &mut pins {
        pin.minimum_head = snapshot.state().head(&pin.domain).cloned();
    }
    let snapshot = snapshot.with_independent_pins(&pins)?;
    let original = &prepared.authorization;
    let [resource] = original.resources.as_slice() else {
        return Err(AuthorizationError::InvalidProof(
            "node enrollment requires one confirmed selected resource",
        ));
    };
    let selected = ResourceRef {
        space: resource.payload.space.clone(),
        resource: resource.payload.resource.clone(),
    };
    let authorization = match verify_client_policy_state_selected(
        snapshot.state().clone(),
        &ClientAuthorizationBundle {
            journal: Vec::new(),
            grant_leaf: original.grant_leaf.clone(),
        },
        &pins,
        original.scope.account_id.clone(),
        original.scope.space_id.clone(),
        snapshot
            .state()
            .find_device(&original.scope.device_id)?
            .payload
            .public_key
            .clone(),
        Some(&selected),
        now,
    ) {
        Ok(mut authorization) => {
            authorization.policy_journal = snapshot.journal().to_vec();
            authorization.imported_snapshot = Some(snapshot.clone());
            authorization.client_policy_context = original.client_policy_context.clone();
            Some(authorization)
        }
        Err(AuthorizationError::Revoked | AuthorizationError::Expired) => None,
        Err(error) => return Err(error),
    };
    Ok(VerifiedAccountNodeEnrollment {
        installation: AccountNodeInstallation {
            request: prepared.request.clone(),
            committed: committed.clone(),
        },
        snapshot,
        authorization,
    })
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen(js_name = finishAccountNodeEnrollment))]
pub fn finish_account_node_enrollment(
    prepared: &PreparedAccountNodeEnrollmentCommit,
    committed_journal_json: &str,
    now_ms: f64,
) -> crate::Result<VerifiedAccountNodeEnrollment> {
    let _signature_scope = crate::signature_verification::ScopedSignatureVerification::new();
    let verify = || -> Result<VerifiedAccountNodeEnrollment> {
        finish(
            prepared,
            decode_bounded(committed_journal_json, MAX_PROOF_BYTES)?,
            timestamp(now_ms)?,
        )
    };
    verify().map_err(|e| e.to_string())
}

pub struct VerifiedAccountNodeInstallation {
    policy: VerifiedPolicySnapshot,
    accepted_prefix_len: usize,
    account: AccountId,
    requesting_device: DeviceId,
    node_public_key: PublicKey,
    claim_nonce: Nonce,
    request_hash: ObjectHash,
    input_hash: ObjectHash,
    proof_usage: ExecutionProofUsage,
}
impl VerifiedAccountNodeInstallation {
    pub fn accepted_prefix_len(&self) -> usize {
        self.accepted_prefix_len
    }
    pub fn account(&self) -> &AccountId {
        &self.account
    }
    pub fn requesting_device(&self) -> &DeviceId {
        &self.requesting_device
    }
    pub fn node_public_key(&self) -> &PublicKey {
        &self.node_public_key
    }
    pub fn claim_nonce(&self) -> &Nonce {
        &self.claim_nonce
    }
    pub fn request_hash(&self) -> &ObjectHash {
        &self.request_hash
    }
    pub fn input_hash(&self) -> &ObjectHash {
        &self.input_hash
    }
    pub fn pins(&self) -> &[PolicyPin] {
        self.policy.pins()
    }
    pub fn proof_usage(&self) -> &ExecutionProofUsage {
        &self.proof_usage
    }
    pub fn into_store_parts(self) -> (VerifiedPolicySet, Vec<PolicyPin>, Vec<JournalEntry>) {
        self.policy.into_parts()
    }
}

pub fn verify_account_node_installation(
    context: &VerifiedAccountNodeEnrollmentContext,
    installation_json: &str,
    node: &Identity,
    expected_claim_nonce: &str,
    now_ms: f64,
) -> crate::Result<VerifiedAccountNodeInstallation> {
    let _scope = crate::signature_verification::ScopedSignatureVerification::new();
    let verify = || -> Result<VerifiedAccountNodeInstallation> {
        let installation: AccountNodeInstallation =
            decode_bounded(installation_json, MAX_PROOF_BYTES)?;
        require(
            installation.request.intent == context.credential.intent
                && installation.request.descriptor == context.credential.descriptor,
            "node installation changed its completed purpose-bound context",
        )?;
        let accepted = installation.committed.commit.payload.accepted_at_ms;
        validate_claim(
            &installation.request.intent.payload.claim,
            &PublicKey::of(node),
            &Nonce::parse(expected_claim_nonce)?,
            accepted,
        )?;
        let delta = vec![
            JournalEntry::Device(installation.request.certificate.clone()),
            JournalEntry::AccountCommit(installation.committed.clone()),
        ];
        let now = timestamp(now_ms)?;
        require(
            installation.committed.proposal == installation.request.proposal
                && context.approved_at_ms <= accepted
                && accepted <= now,
            "node installation changed the approved account proposal or acceptance clock",
        )?;
        verify_account_node_enrollment_proposal(
            context.policy.state(),
            &installation.request,
            accepted,
        )?;
        let policy = context.policy.apply_delta(&delta)?;
        policy
            .state()
            .verify_transport_device(&DeviceId::from_key(&PublicKey::of(node)), now)?;
        policy.state().verify_account_client_device(
            &installation.request.intent.payload.account,
            &installation.request.intent.payload.requesting_device,
            now,
        )?;
        let mut pins = policy.pins().to_vec();
        for pin in &mut pins {
            pin.minimum_head = policy.state().head(&pin.domain).cloned();
        }
        let policy = policy.with_independent_pins(&pins)?;
        let mut proof_usage = verify_account_node_enrollment_read(
            context.policy.state(),
            &installation.request.intent,
            &installation.request.descriptor,
            &installation.request.descriptor.source_node_public_key,
            &context.reader,
            accepted,
        )?;
        proof_usage.record_metadata(&installation)?;
        proof_usage.record_signed(&installation.request.certificate)?;
        proof_usage.record_signed(&installation.committed.proposal)?;
        proof_usage.record_signed(&installation.committed.commit)?;
        for receipt in &installation.committed.receipts {
            proof_usage.record_signed(receipt)?;
        }
        proof_usage.enforce_limits()?;
        Ok(VerifiedAccountNodeInstallation {
            accepted_prefix_len: context.policy.journal().len(),
            account: installation.request.intent.payload.account.clone(),
            requesting_device: installation
                .request
                .intent
                .payload
                .requesting_device
                .clone(),
            node_public_key: PublicKey::of(node),
            claim_nonce: installation
                .request
                .intent
                .payload
                .claim
                .payload
                .claim_nonce
                .clone(),
            request_hash: ObjectHash::body(&installation.request.intent)?,
            input_hash: ObjectHash::body(&installation)?,
            proof_usage,
            policy,
        })
    };
    verify().map_err(|e| e.to_string())
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen)]
impl VerifiedAccountNodeEnrollment {
    pub fn authorization(&self) -> Option<VerifiedClientAuthorization> {
        self.authorization.clone()
    }
    #[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen(js_name = pinsJson))]
    pub fn pins_json(&self) -> crate::Result<String> {
        encode(self.snapshot.pins())
    }
    #[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen(js_name = installationJson))]
    pub fn installation_json(&self) -> crate::Result<String> {
        encode(&self.installation)
    }
}
