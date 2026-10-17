//! Add one personal service Node without granting it business or account management rights.

use super::*;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct AccountNodeEnrollmentIntent {
    pub version: u16,
    pub account: AccountId,
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
pub struct AccountNodeEnrollmentCommitRequest {
    pub intent: Signed<AccountNodeEnrollmentIntent>,
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
    pins: Vec<PolicyPin>,
    bundle: ClientAuthorizationBundle,
}

#[derive(Clone, Debug)]
#[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen)]
pub struct PreparedAccountNodeEnrollmentCommit {
    request: AccountNodeEnrollmentCommitRequest,
    pins: Vec<PolicyPin>,
    bundle: ClientAuthorizationBundle,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct AccountNodeInstallation {
    request: AccountNodeEnrollmentCommitRequest,
    pins: Vec<PolicyPin>,
    bundle: ClientAuthorizationBundle,
    committed: Committed<AccountDocument>,
}

#[derive(Clone, Debug)]
#[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen)]
pub struct VerifiedAccountNodeEnrollment {
    installation: AccountNodeInstallation,
    pins: Vec<PolicyPin>,
    bundle: ClientAuthorizationBundle,
}
impl VerifiedAccountNodeEnrollment {
    pub fn pins(&self) -> &[PolicyPin] {
        &self.pins
    }
    pub fn bundle(&self) -> &ClientAuthorizationBundle {
        &self.bundle
    }
    pub fn journal(&self) -> &[JournalEntry] {
        &self.bundle.journal
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
) -> Result<()> {
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
    fresh_node_key(state, &c.public_key)
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
    require(
        canonical(request)?.len() <= MAX_PROOF_BYTES,
        "node enrollment proposal too large",
    )?;
    verify_intent(state, &request.intent, now_ms)?;
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
    pins_json: &str,
    bundle_json: &str,
    claim: &VerifiedNodeClaim,
    certificate_expires_at_ms: f64,
    now_ms: f64,
) -> crate::Result<PreparedAccountNodeEnrollment> {
    let prepare = || -> Result<PreparedAccountNodeEnrollment> {
        let now = timestamp(now_ms)?;
        let (account, root, state, pins, bundle) =
            current_root_material_context(material_json, pins_json, bundle_json)?;
        require(
            bundle.journal.len() <= 126,
            "node enrollment journal requires checkpoint",
        )?;
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
        let intent = Signed::sign(
            AccountNodeEnrollmentIntent {
                version: AUTH_VERSION,
                account: account.clone(),
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
        verify_intent(&state, &intent, now)?;
        Ok(PreparedAccountNodeEnrollment {
            intent,
            pins,
            bundle,
        })
    };
    prepare().map_err(|e| e.to_string())
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen)]
impl PreparedAccountNodeEnrollment {
    #[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen(js_name = requestJson))]
    pub fn request_json(&self) -> crate::Result<String> {
        encode(&self.intent)
    }
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen(js_name = signAccountNodeEnrollmentPossession))]
pub fn sign_account_node_enrollment_possession(
    request_json: &str,
    node: &Identity,
    expected_claim_nonce: &str,
    pins_json: &str,
    bundle_json: &str,
    now_ms: f64,
) -> crate::Result<String> {
    let sign = || -> Result<String> {
        let request: Signed<AccountNodeEnrollmentIntent> = decode_signed(request_json.as_bytes())?;
        let pins: Vec<PolicyPin> = decode_bounded(pins_json, MAX_PROOF_BYTES)?;
        let bundle = read_bundle(bundle_json)?;
        let state = replay_policies(&pins, &bundle.journal)?;
        let now = timestamp(now_ms)?;
        verify_intent(&state, &request, now)?;
        validate_claim(
            &request.payload.claim,
            &PublicKey::of(node),
            &Nonce::parse(expected_claim_nonce)?,
            now,
        )?;
        let mut certificate = request.payload.certificate;
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
    let prepare = || -> Result<PreparedAccountNodeEnrollmentCommit> {
        let now = timestamp(now_ms)?;
        let (account, root, mut state, _, _) = current_root_material_context(
            material_json,
            &encode(&prepared.pins)
                .map_err(|_| AuthorizationError::InvalidProof("invalid enrollment pins"))?,
            &encode(&prepared.bundle)
                .map_err(|_| AuthorizationError::InvalidProof("invalid enrollment bundle"))?,
        )?;
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
            certificate,
            proposal,
        };
        let state = replay_policies(&prepared.pins, &prepared.bundle.journal)?;
        verify_account_node_enrollment_proposal(&state, &request, now)?;
        Ok(PreparedAccountNodeEnrollmentCommit {
            request,
            pins: prepared.pins.clone(),
            bundle: prepared.bundle.clone(),
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

#[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen(js_name = commitAccountNodeEnrollment))]
pub fn commit_account_node_enrollment(
    request_json: &str,
    authority: &Identity,
    pins_json: &str,
    bundle_json: &str,
    now_ms: f64,
) -> crate::Result<String> {
    let commit = || -> Result<Vec<JournalEntry>> {
        let request: AccountNodeEnrollmentCommitRequest =
            decode_bounded(request_json, MAX_PROOF_BYTES)?;
        let pins: Vec<PolicyPin> = decode_bounded(pins_json, MAX_PROOF_BYTES)?;
        let bundle = read_bundle(bundle_json)?;
        let mut state = replay_policies(&pins, &bundle.journal)?;
        let now = timestamp(now_ms)?;
        let verified = verify_account_node_enrollment_proposal(&state, &request, now)?;
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
            proposal: request.proposal,
            commit,
            receipts: vec![receipt],
        };
        state.install_device(request.certificate.clone(), now)?;
        state.install_account_committed(committed.clone(), now)?;
        Ok(vec![
            JournalEntry::Device(request.certificate),
            JournalEntry::AccountCommit(committed),
        ])
    };
    encode(&commit().map_err(|e| e.to_string())?)
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
    let mut state = replay_policies(&prepared.pins, &prepared.bundle.journal)?;
    verify_account_node_enrollment_proposal(
        &state,
        &prepared.request,
        committed.commit.payload.accepted_at_ms,
    )?;
    state.install_device(certificate.clone(), committed.commit.payload.accepted_at_ms)?;
    let head = state.replay_account_committed(committed.clone())?;
    state.verify_transport_device(&certificate.payload.device, now)?;
    let mut pins = prepared.pins.clone();
    let pin = pins
        .iter_mut()
        .find(|pin| pin.domain == head.domain)
        .ok_or(AuthorizationError::Revoked)?;
    pin.minimum_head = Some(head);
    let mut bundle = prepared.bundle.clone();
    bundle.journal.extend(delta.clone());
    replay_policies(&pins, &bundle.journal)?;
    Ok(VerifiedAccountNodeEnrollment {
        installation: AccountNodeInstallation {
            request: prepared.request.clone(),
            pins: prepared.pins.clone(),
            bundle: prepared.bundle.clone(),
            committed: committed.clone(),
        },
        pins,
        bundle,
    })
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen(js_name = finishAccountNodeEnrollment))]
pub fn finish_account_node_enrollment(
    prepared: &PreparedAccountNodeEnrollmentCommit,
    committed_journal_json: &str,
    now_ms: f64,
) -> crate::Result<VerifiedAccountNodeEnrollment> {
    let verify = || -> Result<VerifiedAccountNodeEnrollment> {
        finish(
            prepared,
            decode_bounded(committed_journal_json, MAX_RECOVERY_JOURNAL_BYTES)?,
            timestamp(now_ms)?,
        )
    };
    verify().map_err(|e| e.to_string())
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen(js_name = verifyAccountNodeInstallation))]
pub fn verify_account_node_installation(
    installation_json: &str,
    node: &Identity,
    expected_claim_nonce: &str,
    now_ms: f64,
) -> crate::Result<VerifiedAccountNodeEnrollment> {
    let verify = || -> Result<VerifiedAccountNodeEnrollment> {
        let installation: AccountNodeInstallation = decode_bounded(installation_json, 2_097_152)?;
        let accepted = installation.committed.commit.payload.accepted_at_ms;
        validate_claim(
            &installation.request.intent.payload.claim,
            &PublicKey::of(node),
            &Nonce::parse(expected_claim_nonce)?,
            accepted,
        )?;
        let delta = vec![
            JournalEntry::Device(installation.request.certificate.clone()),
            JournalEntry::AccountCommit(installation.committed),
        ];
        let prepared = PreparedAccountNodeEnrollmentCommit {
            request: installation.request,
            pins: installation.pins,
            bundle: installation.bundle,
        };
        finish(&prepared, delta, timestamp(now_ms)?)
    };
    verify().map_err(|e| e.to_string())
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen)]
impl VerifiedAccountNodeEnrollment {
    #[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen(js_name = pinsJson))]
    pub fn pins_json(&self) -> crate::Result<String> {
        encode(&self.pins)
    }
    #[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen(js_name = bundleJson))]
    pub fn bundle_json(&self) -> crate::Result<String> {
        encode(&self.bundle)
    }
    #[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen(js_name = installationJson))]
    pub fn installation_json(&self) -> crate::Result<String> {
        encode(&self.installation)
    }
}
