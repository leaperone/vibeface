//! Current account enrollment eligibility and signed device policy changes.

use super::{
    client_policy_import::require_confirmed_client_policy, policy_import::VerifiedPolicySnapshot, *,
};

pub mod offer;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct DeviceEnrollmentIntent {
    pub account: AccountId,
    pub new_device_public_key: PublicKey,
    pub requested_rights: Vec<AccountRight>,
    pub certificate_not_before_ms: u64,
    pub certificate_expires_at_ms: u64,
    pub target: DeviceId,
    pub expected_heads: Vec<PolicyStamp>,
}
impl sealed::Sealed for DeviceEnrollmentIntent {}
impl SignedObject for DeviceEnrollmentIntent {
    const DOMAIN: &'static str = "device-enrollment-intent-v2";
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct AuthenticatorEnrollmentIntent {
    pub account: AccountId,
    pub requesting_device_public_key: PublicKey,
    pub target: DeviceId,
    pub credential_not_before_ms: u64,
    pub credential_expires_at_ms: u64,
    pub expected_heads: Vec<PolicyStamp>,
}
impl sealed::Sealed for AuthenticatorEnrollmentIntent {}
impl SignedObject for AuthenticatorEnrollmentIntent {
    const DOMAIN: &'static str = "authenticator-enrollment-intent-v2";
}

macro_rules! qualification {
    ($name:ident, $intent:ty) => {
        #[derive(Clone, Debug)]
        #[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen)]
        pub struct $name {
            signed: Signed<$intent>,
            hash: ObjectHash,
            expires: u64,
        }
        impl $name {
            pub fn intent(&self) -> &$intent { &self.signed.payload }
            pub fn intent_hash(&self) -> &ObjectHash { &self.hash }
            pub fn account(&self) -> &AccountId { &self.signed.payload.account }
            pub fn issuer(&self) -> &PublicKey { &self.signed.issuer }
            pub fn expected_heads(&self) -> &[PolicyStamp] { &self.signed.payload.expected_heads }
            pub fn expires_at_ms(&self) -> u64 { self.expires }
        }
        #[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen)]
        impl $name {
            #[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen(js_name = intentJson))]
            pub fn intent_json(&self) -> crate::Result<String> { encode(self.intent()) }
            #[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen(js_name = intentHash))]
            pub fn intent_hash_string(&self) -> String { self.hash.to_string() }
            #[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen(js_name = expiresAtMs))]
            pub fn expires_at_ms_number(&self) -> f64 { self.expires as f64 }
        }
    };
}
qualification!(VerifiedAccountEnrollment, DeviceEnrollmentIntent);
qualification!(
    VerifiedAuthenticatorEnrollment,
    AuthenticatorEnrollmentIntent
);

qualification!(VerifiedPasskeyEnrollmentRequest, DeviceEnrollmentIntent);

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PasskeyEnrollmentProof {
    pub intent: Signed<DeviceEnrollmentIntent>,
    pub certificate: Signed<DeviceCertificate>,
    pub assertion: crate::webauthn::PasskeyAssertionProof,
}

#[derive(Clone, Debug)]
pub struct VerifiedEnrollment {
    proof: PasskeyEnrollmentProof,
    assertion: crate::webauthn::VerifiedPasskeyAssertion,
    hash: ObjectHash,
    expires: u64,
}
impl VerifiedEnrollment {
    pub fn intent(&self) -> &DeviceEnrollmentIntent {
        &self.proof.intent.payload
    }
    pub fn intent_hash(&self) -> &ObjectHash {
        &self.hash
    }
    pub fn account(&self) -> &AccountId {
        &self.intent().account
    }
    pub fn issuer(&self) -> &PublicKey {
        &self.proof.intent.issuer
    }
    pub fn expected_heads(&self) -> &[PolicyStamp] {
        &self.intent().expected_heads
    }
    pub fn expires_at_ms(&self) -> u64 {
        self.expires
    }
    pub fn certificate(&self) -> &Signed<DeviceCertificate> {
        &self.proof.certificate
    }
    pub fn verified_assertion(&self) -> &crate::webauthn::VerifiedPasskeyAssertion {
        &self.assertion
    }
    pub fn record_hash(&self) -> &ObjectHash {
        self.assertion.record_hash()
    }
}

fn encode<T: Serialize>(value: &T) -> crate::Result<String> {
    serde_json::to_string(value).map_err(|_| "enrollment encoding failed".to_owned())
}
fn parse<T: for<'de> Deserialize<'de>>(json: &str) -> Result<T> {
    require(json.len() <= 1_048_576, "enrollment JSON too large")?;
    serde_json::from_str(json)
        .map_err(|_| AuthorizationError::InvalidProof("invalid enrollment JSON"))
}
fn time(value: f64) -> Result<u64> {
    require(
        value.is_finite()
            && value >= 0.0
            && value.fract() == 0.0
            && value <= 9_007_199_254_740_991.0,
        "invalid enrollment timestamp",
    )?;
    Ok(value as u64)
}
fn confirmed_policy(
    authorization: &VerifiedClientAuthorization,
    now: u64,
) -> Result<&VerifiedPolicySnapshot> {
    let snapshot = require_confirmed_client_policy(authorization)?;
    let resource = authorization
        .resources
        .first()
        .ok_or(AuthorizationError::PolicyConflict)?;
    require(
        authorization.resources.len() == 1,
        "device change requires one selected client resource",
    )?;
    let selected = ResourceRef {
        space: resource.payload.space.clone(),
        resource: resource.payload.resource.clone(),
    };
    let key = snapshot
        .state()
        .find_device(&authorization.scope.device_id)?
        .payload
        .public_key
        .clone();
    let current = verify_client_policy_state_selected(
        snapshot.state().clone(),
        &ClientAuthorizationBundle {
            journal: Vec::new(),
            grant_leaf: authorization.grant_leaf.clone(),
        },
        snapshot.pins(),
        authorization.scope.account_id.clone(),
        authorization.scope.space_id.clone(),
        key,
        Some(&selected),
        now,
    )?;
    require(
        current.heads == authorization.heads && current.audience == authorization.audience,
        "device change requires a fresh confirmed client selection",
    )?;
    Ok(snapshot)
}
fn authorized_account(
    authorization: &VerifiedClientAuthorization,
    account: &AccountId,
) -> Result<()> {
    require(
        authorization.scope.account_id == *account,
        "device change belongs to another authenticated account",
    )
}
fn current_heads(
    state: &VerifiedPolicySet,
    account: &AccountId,
    heads: &[PolicyStamp],
) -> Result<()> {
    require(
        !heads.is_empty()
            && heads.len() <= MAX_POLICY_DOMAINS
            && heads.windows(2).all(|p| p[0].domain < p[1].domain)
            && heads
                .iter()
                .any(|h| h.domain == PolicyDomain::Account(account.clone()))
            && heads.iter().all(|h| state.head(&h.domain) == Some(h)),
        "enrollment policy heads changed",
    )
}
enum EnrollmentActor<'a> {
    Root,
    Device {
        authority: &'a EnrollmentAuthority,
        certificate: &'a DeviceCertificate,
    },
}
fn actor<'a>(
    state: &'a VerifiedPolicySet,
    account: &AccountId,
    issuer: &PublicKey,
    right: AccountRight,
    now: u64,
) -> Result<EnrollmentActor<'a>> {
    actor_in(state, account, issuer, right, now, &mut Budget::default())
}
fn actor_in<'a>(
    state: &'a VerifiedPolicySet,
    account: &AccountId,
    issuer: &PublicKey,
    right: AccountRight,
    now: u64,
    budget: &mut Budget,
) -> Result<EnrollmentActor<'a>> {
    let document = state
        .account(account)
        .ok_or(AuthorizationError::PolicyConflict)?;
    budget.verify(
        state
            .accounts
            .get(account)
            .and_then(|account| account.history.last())
            .ok_or(AuthorizationError::PolicyConflict)?,
    )?;
    if *issuer == document.current_root {
        return Ok(EnrollmentActor::Root);
    }
    let certificate = state.find_device(&DeviceId::from_key(issuer))?;
    require(
        certificate.payload.subject == Actor::Account(account.clone())
            && certificate.payload.public_key == *issuer
            && (right != AccountRight::EnrollDevice
                || certificate.payload.uses.contains(&DeviceUse::EnrollDevice)),
        "enrollment actor lacks device use",
    )?;
    state.validate_certificate(
        certificate,
        now,
        budget,
        &mut BTreeMap::new(),
        &mut BTreeSet::new(),
    )?;
    let authority = document
        .enrollment
        .iter()
        .find(|e| {
            e.device == certificate.payload.device
                && now < e.expires_at_ms
                && e.rights.contains(&right)
        })
        .ok_or(AuthorizationError::Revoked)?;
    Ok(EnrollmentActor::Device {
        authority,
        certificate: &certificate.payload,
    })
}
fn deadline(role: &EnrollmentActor<'_>, requested: u64, now: u64) -> u64 {
    let expires = requested.min(now.saturating_add(INVOCATION_MAX_AGE_MS));
    match role {
        EnrollmentActor::Root => expires,
        EnrollmentActor::Device {
            authority,
            certificate,
        } => expires
            .min(authority.expires_at_ms)
            .min(certificate.expires_at_ms),
    }
}

fn independent_device_key(
    state: &VerifiedPolicySet,
    account: &AccountId,
    public_key: &PublicKey,
) -> Result<()> {
    let history = state
        .account_history(account)
        .ok_or(AuthorizationError::PolicyConflict)?;
    require(
        history.iter().all(|document| {
            document.payload.genesis_root != *public_key
                && document.payload.current_root != *public_key
                && !document.payload.recovery.keys.contains(public_key)
                && !document
                    .payload
                    .revoked_devices
                    .contains(&DeviceId::from_key(public_key))
        }) && !state
            .devices
            .values()
            .any(|certificate| certificate.payload.public_key == *public_key),
        "enrollment requires an independent new device key",
    )
}

pub fn verify_account_enrollment(
    state: &VerifiedPolicySet,
    signed: &Signed<DeviceEnrollmentIntent>,
    now_ms: u64,
) -> Result<VerifiedAccountEnrollment> {
    let _signature_scope = crate::signature_verification::ScopedSignatureVerification::new();
    signed.verify_signature()?;
    let i = &signed.payload;
    current_heads(state, &i.account, &i.expected_heads)?;
    validate_window(
        i.certificate_not_before_ms,
        i.certificate_expires_at_ms,
        now_ms,
    )?;
    require(
        i.requested_rights.len() <= 3 && sorted_unique(&i.requested_rights),
        "invalid requested enrollment rights",
    )?;
    independent_device_key(state, &i.account, &i.new_device_public_key)?;
    let role = actor(
        state,
        &i.account,
        &signed.issuer,
        AccountRight::EnrollDevice,
        now_ms,
    )?;
    if let EnrollmentActor::Device {
        authority,
        certificate,
    } = &role
    {
        require(
            i.requested_rights
                .iter()
                .all(|right| authority.rights.contains(right))
                && i.certificate_not_before_ms >= certificate.not_before_ms
                && i.certificate_expires_at_ms <= authority.expires_at_ms
                && i.certificate_expires_at_ms <= certificate.expires_at_ms,
            "requested device authority exceeds current manager",
        )?;
    }
    Ok(VerifiedAccountEnrollment {
        signed: signed.clone(),
        hash: ObjectHash::body(i)?,
        expires: deadline(&role, i.certificate_expires_at_ms, now_ms),
    })
}

pub fn verify_authenticator_enrollment(
    state: &VerifiedPolicySet,
    signed: &Signed<AuthenticatorEnrollmentIntent>,
    now_ms: u64,
) -> Result<VerifiedAuthenticatorEnrollment> {
    let _signature_scope = crate::signature_verification::ScopedSignatureVerification::new();
    signed.verify_signature()?;
    let i = &signed.payload;
    current_heads(state, &i.account, &i.expected_heads)?;
    validate_window(
        i.credential_not_before_ms,
        i.credential_expires_at_ms,
        now_ms,
    )?;
    let role = actor(
        state,
        &i.account,
        &signed.issuer,
        AccountRight::ManageAuthenticators,
        now_ms,
    )?;
    let certificate = state.find_device(&DeviceId::from_key(&i.requesting_device_public_key))?;
    require(
        certificate.payload.subject == Actor::Account(i.account.clone())
            && certificate.payload.public_key == i.requesting_device_public_key
            && (matches!(role, EnrollmentActor::Root)
                || signed.issuer == i.requesting_device_public_key),
        "authenticator requester is not its current account device",
    )?;
    state.validate_certificate(
        certificate,
        now_ms,
        &mut Budget::default(),
        &mut BTreeMap::new(),
        &mut BTreeSet::new(),
    )?;
    let expires =
        deadline(&role, i.credential_expires_at_ms, now_ms).min(certificate.payload.expires_at_ms);
    Ok(VerifiedAuthenticatorEnrollment {
        signed: signed.clone(),
        hash: ObjectHash::body(i)?,
        expires,
    })
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen(js_name = verifyAccountEnrollment))]
pub fn verify_account_enrollment_json(
    signed_json: &str,
    authorization: &VerifiedClientAuthorization,
    now_ms: f64,
) -> crate::Result<VerifiedAccountEnrollment> {
    let _signature_scope = crate::signature_verification::ScopedSignatureVerification::new();
    let verify = || -> Result<VerifiedAccountEnrollment> {
        let snapshot = confirmed_policy(authorization, time(now_ms)?)?;
        let state = snapshot.state();
        let signed: Signed<DeviceEnrollmentIntent> = decode_signed(signed_json.as_bytes())?;
        authorized_account(authorization, &signed.payload.account)?;
        verify_account_enrollment(&state, &signed, time(now_ms)?)
    };
    verify().map_err(|e| e.to_string())
}
#[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen(js_name = verifyAuthenticatorEnrollment))]
pub fn verify_authenticator_enrollment_json(
    signed_json: &str,
    authorization: &VerifiedClientAuthorization,
    now_ms: f64,
) -> crate::Result<VerifiedAuthenticatorEnrollment> {
    let _signature_scope = crate::signature_verification::ScopedSignatureVerification::new();
    let verify = || -> Result<VerifiedAuthenticatorEnrollment> {
        let snapshot = confirmed_policy(authorization, time(now_ms)?)?;
        let state = snapshot.state();
        let signed: Signed<AuthenticatorEnrollmentIntent> = decode_signed(signed_json.as_bytes())?;
        authorized_account(authorization, &signed.payload.account)?;
        verify_authenticator_enrollment(&state, &signed, time(now_ms)?)
    };
    verify().map_err(|e| e.to_string())
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen(js_name = createDeviceEnrollmentIntent))]
pub fn create_device_enrollment_intent(
    new_device: &Identity,
    account: &str,
    target_node_public_key: &str,
    requested_rights_json: &str,
    expected_heads_json: &str,
    certificate_expires_at_ms: f64,
    now_ms: f64,
) -> crate::Result<String> {
    let create = || -> Result<DeviceEnrollmentIntent> {
        let now = time(now_ms)?;
        let expires = time(certificate_expires_at_ms)?;
        validate_window(now, expires, now)?;
        let rights: Vec<AccountRight> = parse(requested_rights_json)?;
        require(
            rights.len() <= 3 && sorted_unique(&rights),
            "invalid requested enrollment rights",
        )?;
        Ok(DeviceEnrollmentIntent {
            account: AccountId::parse(account)?,
            new_device_public_key: PublicKey::of(new_device),
            requested_rights: rights,
            certificate_not_before_ms: now,
            certificate_expires_at_ms: expires,
            target: DeviceId::from_key(&PublicKey::parse(target_node_public_key)?),
            expected_heads: parse(expected_heads_json)?,
        })
    };
    encode(&create().map_err(|e| e.to_string())?)
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen(js_name = signDeviceEnrollmentIntent))]
pub fn sign_device_enrollment_intent(
    intent_json: &str,
    signer: &Identity,
    authorization: &VerifiedClientAuthorization,
    now_ms: f64,
) -> crate::Result<String> {
    let _signature_scope = crate::signature_verification::ScopedSignatureVerification::new();
    let sign = || -> Result<Signed<DeviceEnrollmentIntent>> {
        let snapshot = confirmed_policy(authorization, time(now_ms)?)?;
        let state = snapshot.state();
        let intent: DeviceEnrollmentIntent = parse(intent_json)?;
        authorized_account(authorization, &intent.account)?;
        let signed = Signed::sign(intent, signer)?;
        verify_account_enrollment(&state, &signed, time(now_ms)?)?;
        Ok(signed)
    };
    encode(&sign().map_err(|e| e.to_string())?)
}
#[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen(js_name = signAuthenticatorEnrollmentIntent))]
pub fn sign_authenticator_enrollment_intent(
    intent_json: &str,
    signer: &Identity,
    authorization: &VerifiedClientAuthorization,
    now_ms: f64,
) -> crate::Result<String> {
    let _signature_scope = crate::signature_verification::ScopedSignatureVerification::new();
    let sign = || -> Result<Signed<AuthenticatorEnrollmentIntent>> {
        let snapshot = confirmed_policy(authorization, time(now_ms)?)?;
        let state = snapshot.state();
        let intent: AuthenticatorEnrollmentIntent = parse(intent_json)?;
        authorized_account(authorization, &intent.account)?;
        let signed = Signed::sign(intent, signer)?;
        verify_authenticator_enrollment(&state, &signed, time(now_ms)?)?;
        Ok(signed)
    };
    encode(&sign().map_err(|e| e.to_string())?)
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(
    tag = "kind",
    content = "value",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum AccountDeviceChange {
    Enrollment {
        intent: Signed<DeviceEnrollmentIntent>,
        certificate: Signed<DeviceCertificate>,
    },
    Passkey(PasskeyEnrollmentProof),
    Revocation(DeviceId),
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct AccountDevicePolicyRequest {
    pub proposal: Signed<AccountDocument>,
    pub change: AccountDeviceChange,
    pub nonce: Nonce,
}
#[derive(Clone, Debug)]
#[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen)]
pub struct PreparedAccountDeviceChange {
    request: AccountDevicePolicyRequest,
    pins: Vec<PolicyPin>,
    state: VerifiedPolicySet,
    imported_snapshot: Option<VerifiedPolicySnapshot>,
    authorization: Option<VerifiedClientAuthorization>,
}
#[derive(Clone, Debug)]
#[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen)]
pub struct VerifiedAccountDeviceChange {
    pins: Vec<PolicyPin>,
    journal_delta: Vec<JournalEntry>,
    authorization: Option<VerifiedClientAuthorization>,
    scope: Option<ClientScope>,
}

fn next_document(state: &VerifiedPolicySet, account: &AccountId) -> Result<AccountDocument> {
    let mut d = state
        .account(account)
        .ok_or(AuthorizationError::PolicyConflict)?
        .clone();
    d.previous = state.head(&PolicyDomain::Account(account.clone())).cloned();
    d.sequence = d
        .sequence
        .checked_add(1)
        .ok_or(AuthorizationError::PolicyConflict)?;
    d.device_enrollment = None;
    d.authenticator_approval = None;
    d.root_acceptance = None;
    d.recovery_approvals.clear();
    Ok(d)
}
fn enrollment_document(
    state: &mut VerifiedPolicySet,
    intent: &Signed<DeviceEnrollmentIntent>,
    certificate: &Signed<DeviceCertificate>,
    now: u64,
) -> Result<AccountDocument> {
    verify_account_enrollment(state, intent, now)?;
    let i = &intent.payload;
    let mut expected = self::certificate(i, certificate.payload.nonce.clone())?;
    expected.possession = certificate.payload.possession.clone();
    require(
        certificate.payload == expected && certificate.issuer == intent.issuer,
        "device certificate does not match approved intent",
    )?;
    let hash = state.install_device(certificate.clone(), now)?;
    let mut d = next_document(state, &i.account)?;
    d.devices.push(hash);
    d.devices.sort();
    if !i.requested_rights.is_empty() {
        d.enrollment.push(EnrollmentAuthority {
            device: certificate.payload.device.clone(),
            rights: i.requested_rights.clone(),
            expires_at_ms: i.certificate_expires_at_ms,
        });
        d.enrollment.sort_by(|a, b| a.device.cmp(&b.device));
    }
    Ok(d)
}
fn revocation_document(
    state: &VerifiedPolicySet,
    account: &AccountId,
    device: &DeviceId,
    signer: &PublicKey,
    now: u64,
) -> Result<AccountDocument> {
    actor(state, account, signer, AccountRight::RevokeDevice, now)?;
    let target = state.find_device(device)?;
    require(
        target.payload.owner == PolicyDomain::Account(account.clone()),
        "cannot revoke another account device",
    )?;
    let mut d = next_document(state, account)?;
    require(
        d.authority.device != *device,
        "authority revocation requires authority replacement",
    )?;
    d.devices
        .retain(|h| state.device(h).is_none_or(|c| c.payload.device != *device));
    d.revoked_devices.push(device.clone());
    d.revoked_devices.sort();
    d.revoked_devices.dedup();
    d.enrollment.retain(|e| e.device != *device);
    Ok(d)
}
fn validate_device_request(
    state: &mut VerifiedPolicySet,
    request: &AccountDevicePolicyRequest,
    now: u64,
) -> Result<Vec<PolicyStamp>> {
    request.proposal.verify_signature()?;
    let account = &request.proposal.payload.account;
    let previous = state
        .account(account)
        .ok_or(AuthorizationError::PolicyConflict)?
        .clone();
    let mut budget = Budget::default();
    let expected = match &request.change {
        AccountDeviceChange::Enrollment {
            intent,
            certificate,
        } => {
            require(
                intent.payload.account == *account
                    && intent.issuer == request.proposal.issuer
                    && certificate.payload.nonce == request.nonce,
                "enrollment request actor or nonce mismatch",
            )?;
            enrollment_document(state, intent, certificate, now)?
        }
        AccountDeviceChange::Passkey(proof) => {
            require(
                proof.intent.payload.account == *account
                    && proof.certificate.payload.nonce == request.nonce,
                "passkey request account or nonce mismatch",
            )?;
            let verified = verify_passkey_device_enrollment(state, proof, now)?;
            let mut d = next_document(state, account)?;
            d.devices.push(verified.certificate().hash()?);
            d.devices.sort();
            d.device_enrollment = Some(proof.clone());
            d
        }
        AccountDeviceChange::Revocation(device) => {
            revocation_document(state, account, device, &request.proposal.issuer, now)?
        }
    };
    require(
        request.proposal.payload == expected,
        "device request changes unrelated account policy",
    )?;
    if request.proposal.issuer != previous.current_root {
        state.authorize_account_delta(&request.proposal, &previous, now, &mut budget)?;
    }
    let mut dependencies = budget.dependencies.clone();
    state.dependency(&PolicyDomain::Account(account.clone()), &mut dependencies)?;
    state.expand_authorities(now, &mut budget, &mut dependencies)?;
    Ok(dependencies.into_values().collect())
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen(js_name = prepareAccountDeviceEnrollment))]
pub fn prepare_account_device_enrollment(
    qualified: &VerifiedAccountEnrollment,
    device_possession_json: &str,
    signer: &Identity,
    authorization: &VerifiedClientAuthorization,
    now_ms: f64,
) -> crate::Result<PreparedAccountDeviceChange> {
    let _signature_scope = crate::signature_verification::ScopedSignatureVerification::new();
    let prepare = || -> Result<PreparedAccountDeviceChange> {
        let now = time(now_ms)?;
        require(
            now < qualified.expires && PublicKey::of(signer) == *qualified.issuer(),
            "enrollment qualification expired or signer mismatch",
        )?;
        let snapshot = confirmed_policy(authorization, now)?;
        let mut state = snapshot.state().clone();
        authorized_account(authorization, qualified.account())?;
        let certificate =
            Signed::sign(parse::<DeviceCertificate>(device_possession_json)?, signer)?;
        let d = enrollment_document(&mut state, &qualified.signed, &certificate, now)?;
        let request = AccountDevicePolicyRequest {
            proposal: Signed::sign(d, signer)?,
            nonce: certificate.payload.nonce.clone(),
            change: AccountDeviceChange::Enrollment {
                intent: qualified.signed.clone(),
                certificate,
            },
        };
        let mut state = snapshot.state().clone();
        validate_device_request(&mut state, &request, now)?;
        Ok(PreparedAccountDeviceChange {
            request,
            pins: snapshot.pins().to_vec(),
            state: snapshot.state().clone(),
            imported_snapshot: Some(snapshot.clone()),
            authorization: Some(authorization.clone()),
        })
    };
    prepare().map_err(|e| e.to_string())
}
#[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen(js_name = preparePasskeyDeviceEnrollment))]
pub fn prepare_passkey_device_enrollment(
    signed_intent_json: &str,
    signed_certificate_json: &str,
    assertion_proof_json: &str,
    new_device: &Identity,
    authorization: &VerifiedClientAuthorization,
    now_ms: f64,
) -> crate::Result<PreparedAccountDeviceChange> {
    let _signature_scope = crate::signature_verification::ScopedSignatureVerification::new();
    let prepare = || -> Result<PreparedAccountDeviceChange> {
        let now = time(now_ms)?;
        let snapshot = confirmed_policy(authorization, now)?;
        let mut state = snapshot.state().clone();
        let proof = PasskeyEnrollmentProof {
            intent: decode_signed(signed_intent_json.as_bytes())?,
            certificate: decode_signed(signed_certificate_json.as_bytes())?,
            assertion: parse(assertion_proof_json)?,
        };
        authorized_account(authorization, &proof.intent.payload.account)?;
        let request = prepare_passkey_request(&mut state, proof, new_device, now)?;
        Ok(PreparedAccountDeviceChange {
            request,
            pins: snapshot.pins().to_vec(),
            state: snapshot.state().clone(),
            imported_snapshot: Some(snapshot.clone()),
            authorization: Some(authorization.clone()),
        })
    };
    prepare().map_err(|e| e.to_string())
}

fn prepare_passkey_request(
    state: &mut VerifiedPolicySet,
    proof: PasskeyEnrollmentProof,
    new_device: &Identity,
    now: u64,
) -> Result<AccountDevicePolicyRequest> {
    let verified = verify_passkey_device_enrollment(state, &proof, now)?;
    require(
        PublicKey::of(new_device) == *verified.issuer(),
        "passkey enrollment new-device signer mismatch",
    )?;
    let mut document = next_document(state, verified.account())?;
    document.devices.push(verified.certificate().hash()?);
    document.devices.sort();
    document.device_enrollment = Some(proof.clone());
    let request = AccountDevicePolicyRequest {
        proposal: Signed::sign(document, new_device)?,
        nonce: verified.certificate().payload.nonce.clone(),
        change: AccountDeviceChange::Passkey(proof),
    };
    validate_device_request(state, &request, now)?;
    Ok(request)
}
#[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen(js_name = prepareAccountDeviceRevocation))]
pub fn prepare_account_device_revocation(
    account_id: &str,
    revoked_device_id: &str,
    signer: &Identity,
    authorization: &VerifiedClientAuthorization,
    now_ms: f64,
) -> crate::Result<PreparedAccountDeviceChange> {
    let _signature_scope = crate::signature_verification::ScopedSignatureVerification::new();
    let prepare = || -> Result<PreparedAccountDeviceChange> {
        let now = time(now_ms)?;
        let snapshot = confirmed_policy(authorization, now)?;
        let mut state = snapshot.state().clone();
        let account = AccountId::parse(account_id)?;
        authorized_account(authorization, &account)?;
        let device = DeviceId::parse(revoked_device_id)?;
        let d = revocation_document(&state, &account, &device, &PublicKey::of(signer), now)?;
        let request = AccountDevicePolicyRequest {
            proposal: Signed::sign(d, signer)?,
            change: AccountDeviceChange::Revocation(device),
            nonce: Nonce::random()?,
        };
        validate_device_request(&mut state, &request, now)?;
        Ok(PreparedAccountDeviceChange {
            request,
            pins: snapshot.pins().to_vec(),
            state: snapshot.state().clone(),
            imported_snapshot: Some(snapshot.clone()),
            authorization: Some(authorization.clone()),
        })
    };
    prepare().map_err(|e| e.to_string())
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen)]
impl PreparedAccountDeviceChange {
    #[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen(js_name = requestJson))]
    pub fn request_json(&self) -> crate::Result<String> {
        encode(&self.request)
    }
}

pub fn issue_account_device_policy_challenge(
    request_json: &str,
    authority: &Identity,
    state: &VerifiedPolicySet,
    now_ms: f64,
) -> crate::Result<String> {
    let _signature_scope = crate::signature_verification::ScopedSignatureVerification::new();
    let issue = || -> Result<Signed<PolicyCommit>> {
        let now = time(now_ms)?;
        let request: AccountDevicePolicyRequest = parse(request_json)?;
        let mut state = state.clone();
        let dependencies = validate_device_request(&mut state, &request, now)?;
        let account = state
            .account(&request.proposal.payload.account)
            .ok_or(AuthorizationError::PolicyConflict)?;
        require(
            account.authority.public_key == PublicKey::of(authority),
            "policy challenge requires current authority",
        )?;
        Signed::sign(
            PolicyCommit {
                version: AUTH_VERSION,
                previous: state
                    .head(&PolicyDomain::Account(account.account.clone()))
                    .cloned()
                    .ok_or(AuthorizationError::PolicyConflict)?,
                proposal_hash: request.proposal.hash()?,
                accepted_at_ms: 0,
                issued_at_ms: now,
                expires_at_ms: now
                    .checked_add(EXECUTION_LEASE_MS)
                    .ok_or(AuthorizationError::PolicyConflict)?,
                nonce: request.nonce,
                dependencies,
            },
            authority,
        )
    };
    encode(&issue().map_err(|e| e.to_string())?)
}
pub fn accept_account_device_policy_change(
    request_json: &str,
    challenge_json: &str,
    receipts_json: &str,
    authority: &Identity,
    state: &VerifiedPolicySet,
    now_ms: f64,
) -> crate::Result<String> {
    let _signature_scope = crate::signature_verification::ScopedSignatureVerification::new();
    let accept = || -> Result<Committed<AccountDocument>> {
        let now = time(now_ms)?;
        let request: AccountDevicePolicyRequest = parse(request_json)?;
        let mut state = state.clone();
        let dependencies = validate_device_request(&mut state, &request, now)?;
        let challenge: Signed<PolicyCommit> = decode_signed(challenge_json.as_bytes())?;
        challenge.verify_signature()?;
        let account = state
            .account(&request.proposal.payload.account)
            .ok_or(AuthorizationError::PolicyConflict)?;
        require(
            challenge.issuer == PublicKey::of(authority)
                && account.authority.public_key == challenge.issuer
                && challenge.payload.accepted_at_ms == 0
                && challenge.payload.version == AUTH_VERSION
                && challenge.payload.previous
                    == *state
                        .head(&PolicyDomain::Account(account.account.clone()))
                        .ok_or(AuthorizationError::PolicyConflict)?
                && challenge.payload.proposal_hash == request.proposal.hash()?
                && challenge.payload.dependencies == dependencies
                && challenge.payload.nonce == request.nonce,
            "policy challenge does not match current device request",
        )?;
        validate_window(
            challenge.payload.issued_at_ms,
            challenge.payload.expires_at_ms,
            now,
        )?;
        let mut payload = challenge.payload;
        payload.accepted_at_ms = now;
        let committed = Committed {
            proposal: request.proposal,
            commit: Signed::sign(payload, authority)?,
            receipts: parse(receipts_json)?,
        };
        state.install_account_committed(committed.clone(), now)?;
        Ok(committed)
    };
    encode(&accept().map_err(|e| e.to_string())?)
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen(js_name = finishAccountDeviceChange))]
pub fn finish_account_device_change(
    prepared: &PreparedAccountDeviceChange,
    committed_json: &str,
    now_ms: f64,
) -> crate::Result<VerifiedAccountDeviceChange> {
    let _signature_scope = crate::signature_verification::ScopedSignatureVerification::new();
    let finish = || -> Result<VerifiedAccountDeviceChange> {
        let now = time(now_ms)?;
        let committed: Committed<AccountDocument> = parse(committed_json)?;
        let accepted = committed.commit.payload.accepted_at_ms;
        require(
            committed.proposal == prepared.request.proposal && accepted <= now,
            "policy commit belongs to another device request",
        )?;
        let mut state = prepared.state.clone();
        validate_device_request(&mut state, &prepared.request, accepted)?;
        let head = state.replay_account_committed(committed.clone())?;
        let mut delta = Vec::new();
        let key = match &prepared.request.change {
            AccountDeviceChange::Enrollment { certificate, .. } => {
                delta.push(JournalEntry::Device(certificate.clone()));
                Some(certificate.payload.public_key.clone())
            }
            AccountDeviceChange::Passkey(proof) => {
                Some(proof.certificate.payload.public_key.clone())
            }
            AccountDeviceChange::Revocation(_) => None,
        };
        delta.push(JournalEntry::AccountCommit(committed));
        let snapshot = prepared
            .imported_snapshot
            .as_ref()
            .map(|snapshot| snapshot.apply_delta(&delta))
            .transpose()?;
        let mut pins = prepared.pins.clone();
        for pin in &mut pins {
            if pin.domain == head.domain {
                pin.minimum_head = Some(head.clone());
            }
        }
        // A historical receipt can finish after expiry; current writer eligibility is separate.
        let revalidate = |key: PublicKey| -> Result<VerifiedClientAuthorization> {
            let original = prepared
                .authorization
                .as_ref()
                .ok_or(AuthorizationError::Revoked)?;
            let resource = original
                .resources
                .first()
                .ok_or(AuthorizationError::PolicyConflict)?;
            let selected = ResourceRef {
                space: resource.payload.space.clone(),
                resource: resource.payload.resource.clone(),
            };
            let mut current = verify_client_policy_state_selected(
                state.clone(),
                &ClientAuthorizationBundle {
                    journal: Vec::new(),
                    grant_leaf: original.grant_leaf.clone(),
                },
                &pins,
                original.scope.account_id.clone(),
                original.scope.space_id.clone(),
                key,
                Some(&selected),
                now,
            )?;
            current.imported_snapshot = snapshot.clone();
            current.client_policy_context = original.client_policy_context.clone();
            Ok(current)
        };
        let current_or_none = |result: Result<VerifiedClientAuthorization>| -> Result<Option<VerifiedClientAuthorization>> {
            match result {
                Ok(authorization) => Ok(Some(authorization)),
                Err(AuthorizationError::Expired | AuthorizationError::Revoked) => Ok(None),
                Err(error) => Err(error),
            }
        };
        let scope = if prepared.authorization.is_some() {
            key.map(|key| current_or_none(revalidate(key)))
                .transpose()?
                .flatten()
                .map(|auth| auth.scope)
        } else {
            None
        };
        let authorization = if let Some(original) = &prepared.authorization {
            let key = prepared
                .state
                .find_device(&original.scope.device_id)?
                .payload
                .public_key
                .clone();
            current_or_none(revalidate(key))?
        } else {
            None
        };
        Ok(VerifiedAccountDeviceChange {
            pins,
            journal_delta: delta,
            authorization,
            scope,
        })
    };
    finish().map_err(|error| error.to_string())
}
#[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen)]
impl VerifiedAccountDeviceChange {
    #[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen(js_name = journalDeltaJson))]
    pub fn journal_delta_json(&self) -> crate::Result<String> {
        encode(&self.journal_delta)
    }
    pub fn authorization(&self) -> crate::Result<VerifiedClientAuthorization> {
        self.authorization
            .clone()
            .ok_or_else(|| "device change has no current confirmed client authorization".to_owned())
    }
    #[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen(js_name = pinsJson))]
    pub fn pins_json(&self) -> crate::Result<String> {
        encode(&self.pins)
    }
    #[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen(js_name = scopeJson))]
    pub fn scope_json(&self) -> crate::Result<String> {
        encode(&self.scope)
    }
}

fn certificate(intent: &DeviceEnrollmentIntent, nonce: Nonce) -> Result<DeviceCertificate> {
    let mut uses = vec![DeviceUse::Noise, DeviceUse::AccountInvocation];
    if intent
        .requested_rights
        .contains(&AccountRight::EnrollDevice)
    {
        uses.push(DeviceUse::EnrollDevice);
    }
    Ok(DeviceCertificate {
        version: AUTH_VERSION,
        owner: PolicyDomain::Account(intent.account.clone()),
        subject: Actor::Account(intent.account.clone()),
        device: DeviceId::from_key(&intent.new_device_public_key),
        public_key: intent.new_device_public_key.clone(),
        uses,
        possession: None,
        enrollment_grant: None,
        issued_under: intent
            .expected_heads
            .iter()
            .find(|h| h.domain == PolicyDomain::Account(intent.account.clone()))
            .cloned()
            .ok_or(AuthorizationError::PolicyConflict)?,
        nonce,
        not_before_ms: intent.certificate_not_before_ms,
        expires_at_ms: intent.certificate_expires_at_ms,
    })
}
#[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen(js_name = signDeviceEnrollmentPossession))]
pub fn sign_device_enrollment_possession(
    qualified: &VerifiedAccountEnrollment,
    new_device: &Identity,
    pending_nonce: &str,
    now_ms: f64,
) -> crate::Result<String> {
    let sign = || -> Result<DeviceCertificate> {
        let now = time(now_ms)?;
        require(
            now < qualified.expires
                && PublicKey::of(new_device) == qualified.intent().new_device_public_key,
            "enrollment eligibility expired or device mismatch",
        )?;
        let mut c = certificate(qualified.intent(), Nonce::parse(pending_nonce)?)?;
        validate_window(c.not_before_ms, c.expires_at_ms, now)?;
        c.prove_possession(new_device)?;
        Ok(c)
    };
    encode(&sign().map_err(|e| e.to_string())?)
}

pub fn verify_passkey_enrollment_request(
    state: &VerifiedPolicySet,
    signed: &Signed<DeviceEnrollmentIntent>,
    now_ms: u64,
) -> Result<VerifiedPasskeyEnrollmentRequest> {
    let _signature_scope = crate::signature_verification::ScopedSignatureVerification::new();
    signed.verify_signature()?;
    let i = &signed.payload;
    current_heads(state, &i.account, &i.expected_heads)?;
    validate_window(
        i.certificate_not_before_ms,
        i.certificate_expires_at_ms,
        now_ms,
    )?;
    require(
        signed.issuer == i.new_device_public_key && i.requested_rights.is_empty(),
        "passkey enrollment only requests its own ordinary client",
    )?;
    let account = state
        .account(&i.account)
        .ok_or(AuthorizationError::PolicyConflict)?;
    independent_device_key(state, &i.account, &i.new_device_public_key)?;
    let expires = account
        .authenticators
        .iter()
        .filter_map(|hash| state.authenticator(hash))
        .filter(|record| {
            record.payload.client_enrollment == PasskeyClientEnrollment::AccountClient
                && record.payload.not_before_ms <= i.certificate_not_before_ms
                && i.certificate_expires_at_ms <= record.payload.expires_at_ms
                && state.verify_authenticator(record, now_ms).is_ok()
        })
        .map(|record| record.payload.expires_at_ms)
        .max()
        .ok_or(AuthorizationError::UserPresenceRequired)?;
    Ok(VerifiedPasskeyEnrollmentRequest {
        signed: signed.clone(),
        hash: ObjectHash::body(i)?,
        expires: expires
            .min(i.certificate_expires_at_ms)
            .min(now_ms.saturating_add(INVOCATION_MAX_AGE_MS)),
    })
}

pub fn verify_passkey_device_enrollment(
    state: &VerifiedPolicySet,
    proof: &PasskeyEnrollmentProof,
    now_ms: u64,
) -> Result<VerifiedEnrollment> {
    let _signature_scope = crate::signature_verification::ScopedSignatureVerification::new();
    require(
        canonical(proof)?.len() <= MAX_PROOF_BYTES,
        "passkey enrollment proof too large",
    )?;
    let qualified = verify_passkey_enrollment_request(state, &proof.intent, now_ms)?;
    let i = qualified.intent();
    let record = state.verify_authenticator(&proof.assertion.record, now_ms)?;
    require(
        record.account() == &i.account
            && record.record().payload.client_enrollment == PasskeyClientEnrollment::AccountClient,
        "passkey record does not authorize account client enrollment",
    )?;
    let credential = crate::webauthn::RegisteredPasskey::from_authorized(
        &record,
        record.assertion_state(proof.assertion.state)?,
    )
    .map_err(|_| AuthorizationError::InvalidProof("invalid authorized enrollment credential"))?;
    let assertion = crate::webauthn::verify_passkey_assertion(
        &proof.assertion.pending,
        &credential,
        &proof.assertion.response,
        now_ms,
    )
    .map_err(|_| AuthorizationError::InvalidProof("invalid enrollment user verification"))?;
    let envelope = assertion.envelope();
    require(
        envelope.purpose == crate::webauthn::PasskeyPurpose::EnrollDevice
            && envelope.account == i.account
            && envelope.device_public_key == i.new_device_public_key
            && envelope.rights == i.requested_rights
            && envelope.target == i.target
            && envelope.enrollment_hash == *qualified.intent_hash()
            && envelope.expected_heads == i.expected_heads
            && envelope.not_before_ms >= i.certificate_not_before_ms
            && envelope.expires_at_ms <= i.certificate_expires_at_ms
            && i.certificate_not_before_ms >= record.record().payload.not_before_ms
            && i.certificate_expires_at_ms <= record.expires_at_ms(),
        "passkey assertion does not bind enrollment intent",
    )?;
    let mut expected = certificate(i, envelope.nonce.clone())?;
    expected.possession = proof.certificate.payload.possession.clone();
    require(
        proof.certificate.payload == expected
            && proof.certificate.issuer == i.new_device_public_key,
        "passkey certificate changed ordinary client scope",
    )?;
    proof.certificate.verify_signature()?;
    Budget::default().verify_raw(
        &expected.public_key,
        "device-possession-v2",
        &expected.possession_bytes()?,
        expected
            .possession
            .as_ref()
            .ok_or(AuthorizationError::InvalidProof(
                "missing new-device possession",
            ))?,
    )?;
    let expires = qualified
        .expires
        .min(assertion.expires_at_ms())
        .min(record.expires_at_ms());
    Ok(VerifiedEnrollment {
        proof: proof.clone(),
        assertion,
        hash: qualified.hash,
        expires,
    })
}

pub(super) fn authorize_passkey_delta(
    state: &VerifiedPolicySet,
    signed: &Signed<AccountDocument>,
    previous: &AccountDocument,
    at: u64,
    budget: &mut Budget,
) -> Result<()> {
    let proof = signed
        .payload
        .device_enrollment
        .as_ref()
        .ok_or(AuthorizationError::UserPresenceRequired)?;
    budget.verify(&proof.intent)?;
    budget.verify(&proof.certificate)?;
    budget.verify(&proof.assertion.record)?;
    budget.verify_raw(
        &proof.certificate.payload.public_key,
        "device-possession-v2",
        &proof.certificate.payload.possession_bytes()?,
        proof
            .certificate
            .payload
            .possession
            .as_ref()
            .ok_or(AuthorizationError::InvalidProof(
                "missing new-device possession",
            ))?,
    )?;
    budget.external_signature(ObjectHash::body(&proof.assertion)?)?;
    budget.bytes = budget
        .bytes
        .checked_add(canonical(&proof.assertion)?.len())
        .ok_or(AuthorizationError::BudgetExceeded)?;
    if budget.bytes > MAX_PROOF_BYTES {
        return Err(AuthorizationError::BudgetExceeded);
    }
    let verified = verify_passkey_device_enrollment(state, proof, at)?;
    require(
        signed.issuer == *verified.issuer() && signed.payload.account == *verified.account(),
        "passkey delta must be signed by its new client",
    )?;
    let mut expected = previous.clone();
    expected.previous = state
        .head(&PolicyDomain::Account(previous.account.clone()))
        .cloned();
    expected.sequence = previous
        .sequence
        .checked_add(1)
        .ok_or(AuthorizationError::PolicyConflict)?;
    expected.devices.push(proof.certificate.hash()?);
    expected.devices.sort();
    expected.device_enrollment = Some(proof.clone());
    expected.authenticator_approval = None;
    require(
        signed.payload == expected,
        "passkey enrollment changed unrelated account policy",
    )?;
    for stamp in verified.expected_heads() {
        budget
            .dependencies
            .insert(stamp.domain.clone(), stamp.clone());
    }
    budget.expires(verified.expires_at_ms());
    budget.authenticator_assertion(&verified.assertion);
    Ok(())
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen(js_name = signPasskeyEnrollmentIntent))]
pub fn sign_passkey_enrollment_intent(
    intent_json: &str,
    new_device: &Identity,
    authorization: &VerifiedClientAuthorization,
    now_ms: f64,
) -> crate::Result<String> {
    let _signature_scope = crate::signature_verification::ScopedSignatureVerification::new();
    let sign = || -> Result<Signed<DeviceEnrollmentIntent>> {
        let snapshot = confirmed_policy(authorization, time(now_ms)?)?;
        let state = snapshot.state();
        let intent: DeviceEnrollmentIntent = parse(intent_json)?;
        authorized_account(authorization, &intent.account)?;
        let signed = Signed::sign(intent, new_device)?;
        verify_passkey_enrollment_request(&state, &signed, time(now_ms)?)?;
        Ok(signed)
    };
    encode(&sign().map_err(|e| e.to_string())?)
}
#[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen(js_name = verifyPasskeyEnrollmentRequest))]
pub fn verify_passkey_enrollment_request_json(
    signed_json: &str,
    authorization: &VerifiedClientAuthorization,
    now_ms: f64,
) -> crate::Result<VerifiedPasskeyEnrollmentRequest> {
    let _signature_scope = crate::signature_verification::ScopedSignatureVerification::new();
    let verify = || -> Result<VerifiedPasskeyEnrollmentRequest> {
        let snapshot = confirmed_policy(authorization, time(now_ms)?)?;
        let state = snapshot.state();
        let signed: Signed<DeviceEnrollmentIntent> = decode_signed(signed_json.as_bytes())?;
        authorized_account(authorization, &signed.payload.account)?;
        verify_passkey_enrollment_request(&state, &signed, time(now_ms)?)
    };
    verify().map_err(|e| e.to_string())
}
#[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen(js_name = signPasskeyEnrollmentPossession))]
pub fn sign_passkey_enrollment_possession(
    qualified: &VerifiedPasskeyEnrollmentRequest,
    new_device: &Identity,
    pending_nonce: &str,
    now_ms: f64,
) -> crate::Result<String> {
    let sign = || -> Result<Signed<DeviceCertificate>> {
        let now = time(now_ms)?;
        require(
            now < qualified.expires
                && PublicKey::of(new_device) == qualified.intent().new_device_public_key,
            "passkey enrollment eligibility expired or key mismatch",
        )?;
        let mut c = certificate(qualified.intent(), Nonce::parse(pending_nonce)?)?;
        c.prove_possession(new_device)?;
        Signed::sign(c, new_device)
    };
    encode(&sign().map_err(|e| e.to_string())?)
}

#[cfg(test)]
mod tests {
    use super::offer::*;
    use super::*;
    use crate::authorization::bootstrap::*;
    use crate::webauthn::{
        AuthenticationCeremony, AuthenticatorState, EnrollmentEnvelope, PasskeyAssertionProof,
        PasskeyPurpose, PendingPasskey, RelyingParty,
    };
    use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};

    const NOW: u64 = 20_000;
    const END: u64 = 500_000;
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
            &encode(&pins).unwrap(),
            &encode(&Some(&selection)).unwrap(),
            (NOW as f64),
            (NOW as f64) + 60_000.0,
        )
        .unwrap();
        let request: Signed<ClientPolicyReadRequest> = parse(&request_json).unwrap();
        let current = verify_client_policy_read(
            state,
            &request,
            &PublicKey::of(&node),
            &PublicKey::of(&own),
            (NOW as f64) as u64,
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
                issued_at_ms: (NOW as f64) as u64,
                expires_at_ms: (NOW as f64) as u64 + 60_000,
            },
            &node,
        )
        .unwrap();
        let mut import = begin_client_policy_import(
            &encode(&pins).unwrap(),
            &request_json,
            &encode(&snapshot).unwrap(),
            &own,
            &node.public_key_hex(),
            (NOW as f64),
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
                &node,
            )
            .unwrap();
            previous = page.hash().unwrap();
            import
                .append_page(&encode(&page).unwrap(), (NOW as f64))
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
                source_node_public_key: PublicKey::of(&node),
                issued_at_ms: (NOW as f64) as u64,
                expires_at_ms: (NOW as f64) as u64 + 60_000,
            },
            &node,
        )
        .unwrap();
        let authorization = import
            .finish(
                &encode(&witness).unwrap(),
                &encode(&selection).unwrap(),
                (NOW as f64),
            )
            .unwrap();
        let confirmation_request_json = build_client_policy_auth_confirm_request(
            &authorization,
            &own,
            (NOW as f64),
            (NOW as f64) + 30_000.0,
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
            &PublicKey::of(&node),
            &PublicKey::of(&own),
            (NOW as f64) as u64,
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
                accepted_at_ms: (NOW as f64) as u64,
            },
            &node,
        )
        .unwrap();
        confirm_client_policy_authorization(
            &authorization,
            &confirmation_request_json,
            &encode(&confirmation).unwrap(),
            &node.public_key_hex(),
            (NOW as f64),
        )
        .unwrap()
    }
    struct Fixture {
        pins: Vec<PolicyPin>,
        bundle: ClientAuthorizationBundle,
    }
    impl Fixture {
        fn new() -> Self {
            let nonce = Nonce::from_bytes([9; 32]).to_string();
            let claim = verify_node_claim(
                &create_node_claim(&key(4), &nonce, 10_000.0, 100_000.0).unwrap(),
                &key(4).public_key_hex(),
                &nonce,
                10_000.0,
            )
            .unwrap();
            let p =
                prepare_account_creation(&key(1), &key(2), &key(3), &claim, 1_000_000.0, 10_000.0)
                    .unwrap();
            let proof =
                sign_node_possession(&p.node_request_json().unwrap(), &key(4), &nonce, 10_000.0)
                    .unwrap();
            let created = finish_account_creation(&p, &key(1), &proof, 10_000.0).unwrap();
            let mut f = Self {
                pins: parse(&created.pins_json().unwrap()).unwrap(),
                bundle: parse(&created.bundle_json().unwrap()).unwrap(),
            };
            let mut state = f.state();
            let account = f.account();
            let head = state
                .head(&PolicyDomain::Account(account.clone()))
                .unwrap()
                .clone();
            let record = Signed::sign(
                AuthenticatorRecord {
                    version: AUTH_VERSION,
                    account: account.clone(),
                    credential_id: URL_SAFE_NO_PAD.encode([9; 32]),
                    cose_public_key: crate::webauthn::tests::cose(),
                    rp_id: "client.example".to_owned(),
                    origins: vec!["https://client.example".to_owned()],
                    user_handle: URL_SAFE_NO_PAD.encode([7; 32]),
                    backup_eligible: true,
                    initial_state: AuthenticatorState {
                        sign_count: 0,
                        backup_state: false,
                    },
                    client_enrollment: PasskeyClientEnrollment::AccountClient,
                    issued_under: head.clone(),
                    not_before_ms: 10_000,
                    expires_at_ms: 1_000_000,
                    nonce: Nonce::from_bytes([8; 32]),
                    enrollment_hash: ObjectHash::body(&"root-approved fixture").unwrap(),
                },
                &key(1),
            )
            .unwrap();
            let hash = state.install_authenticator(record.clone(), NOW).unwrap();
            let mut d = next_document(&state, &account).unwrap();
            d.authenticators.push(hash);
            if !d.enrollment[0]
                .rights
                .contains(&AccountRight::ManageAuthenticators)
            {
                d.enrollment[0]
                    .rights
                    .push(AccountRight::ManageAuthenticators);
                d.enrollment[0].rights.sort();
            }
            let d = Signed::sign(d, &key(1)).unwrap();
            let head = state.install_account(d.clone()).unwrap();
            f.bundle.journal.extend([
                JournalEntry::Authenticator(record),
                JournalEntry::Account(d),
            ]);
            for pin in &mut f.pins {
                if pin.domain == head.domain {
                    pin.minimum_head = Some(head.clone());
                }
            }
            f
        }
        fn state(&self) -> VerifiedPolicySet {
            replay_policies(&self.pins, &self.bundle.journal).unwrap()
        }
        fn account(&self) -> AccountId {
            AccountId::from_root(&PublicKey::of(&key(1)))
        }
        fn pins_json(&self) -> String {
            encode(&self.pins).unwrap()
        }
        fn bundle_json(&self) -> String {
            encode(&self.bundle).unwrap()
        }
        fn authorization(&self) -> VerifiedClientAuthorization {
            let state = self.state();
            let resource = state
                .resources
                .values()
                .find(|record| {
                    record.payload.kind == ResourceKind::Project
                        && record.payload.space == SpaceId::personal(&self.account())
                })
                .unwrap();
            confirmed_authorization(
                &state,
                &self.pins,
                &self.bundle,
                &ResourceRef {
                    space: resource.payload.space.clone(),
                    resource: resource.payload.resource.clone(),
                },
            )
        }
        fn import_offer(&self, offer_json: &str, reader: &Identity) -> VerifiedEnrollmentOffer {
            self.import_offer_with_evidence(offer_json, reader).0
        }
        fn import_offer_with_evidence(
            &self,
            offer_json: &str,
            reader: &Identity,
        ) -> (VerifiedEnrollmentOffer, serde_json::Value) {
            use crate::authorization::enrollment_policy_import::*;
            let descriptor = verify_device_enrollment_offer(offer_json, NOW as f64).unwrap();
            let request_json = build_enrollment_offer_policy_read_request(
                &descriptor,
                reader,
                NOW as f64,
                (NOW + 60_000) as f64,
            )
            .unwrap();
            let request: Signed<EnrollmentPolicyReadRequest> = parse(&request_json).unwrap();
            let signed_offer: Signed<EnrollmentOffer> = parse(offer_json).unwrap();
            let context = enrollment_offer_policy_descriptor(&signed_offer);
            let snapshot = Signed::sign(
                EnrollmentPolicySnapshot {
                    version: AUTH_VERSION,
                    purpose: EnrollmentPolicyPurpose::BrowserDevice,
                    credential_hash: enrollment_offer_hash(&signed_offer).unwrap(),
                    request_hash: request.hash().unwrap(),
                    snapshot_id: Nonce::random().unwrap(),
                    descriptor_hash: ObjectHash::body(&context).unwrap(),
                    reader_public_key: PublicKey::of(reader),
                    target_public_key: PublicKey::of(reader),
                    source_node_public_key: PublicKey::of(&key(4)),
                    saved_pins_hash: ObjectHash::body(&context.pins).unwrap(),
                    required_heads_hash: ObjectHash::body(&context.required_heads).unwrap(),
                    target_heads: context.target_heads.clone(),
                    issued_at_ms: NOW,
                    expires_at_ms: request.payload.expires_at_ms,
                },
                &key(4),
            )
            .unwrap();
            let mut import = begin_enrollment_offer_import(
                &descriptor,
                &request_json,
                &encode(&snapshot).unwrap(),
                reader,
                &key(4).public_key_hex(),
                NOW as f64,
            )
            .unwrap();
            let mut previous = snapshot.hash().unwrap();
            let mut cursor = None;
            let mut pages = Vec::new();
            for (index, entry) in self.bundle.journal.iter().enumerate() {
                let complete = index + 1 == self.bundle.journal.len();
                let page = Signed::sign(
                    EnrollmentPolicyPage {
                        version: AUTH_VERSION,
                        purpose: EnrollmentPolicyPurpose::BrowserDevice,
                        credential_hash: enrollment_offer_hash(&signed_offer).unwrap(),
                        request_hash: request.hash().unwrap(),
                        snapshot_id: snapshot.payload.snapshot_id.clone(),
                        snapshot_hash: snapshot.hash().unwrap(),
                        request_cursor: cursor,
                        page_index: index as u64,
                        previous_page_hash: previous,
                        entry: Some(entry.clone()),
                        next_cursor: if complete {
                            None
                        } else {
                            Some(ObjectHash::body(&("offer-test-cursor", index)).unwrap())
                        },
                        complete,
                        issued_at_ms: NOW,
                        expires_at_ms: request.payload.expires_at_ms,
                    },
                    &key(4),
                )
                .unwrap();
                previous = page.hash().unwrap();
                cursor = page.payload.next_cursor.clone();
                import
                    .append_page(&encode(&page).unwrap(), NOW as f64)
                    .unwrap();
                pages.push(page);
            }
            let witness = Signed::sign(
                EnrollmentPolicyWitness {
                    version: AUTH_VERSION,
                    purpose: EnrollmentPolicyPurpose::BrowserDevice,
                    credential_hash: enrollment_offer_hash(&signed_offer).unwrap(),
                    request_hash: request.hash().unwrap(),
                    snapshot_id: snapshot.payload.snapshot_id.clone(),
                    snapshot_hash: snapshot.hash().unwrap(),
                    target_heads_hash: ObjectHash::body(&context.target_heads).unwrap(),
                    final_page_hash: previous,
                    page_count: self.bundle.journal.len() as u64,
                    entry_count: self.bundle.journal.len() as u64,
                    issued_at_ms: NOW,
                    expires_at_ms: request.payload.expires_at_ms,
                },
                &key(4),
            )
            .unwrap();
            let verified = import
                .finish(&encode(&witness).unwrap(), NOW as f64)
                .unwrap();
            (
                verified,
                serde_json::json!({"request":request,"snapshot":snapshot,"pages":pages,"witness":witness}),
            )
        }
        fn intent(&self, child: u8, rights: Vec<AccountRight>) -> DeviceEnrollmentIntent {
            DeviceEnrollmentIntent {
                account: self.account(),
                new_device_public_key: PublicKey::of(&key(child)),
                requested_rights: rights,
                certificate_not_before_ms: NOW,
                certificate_expires_at_ms: END,
                target: DeviceId::from_key(&PublicKey::of(&key(4))),
                expected_heads: vec![
                    self.state()
                        .head(&PolicyDomain::Account(self.account()))
                        .unwrap()
                        .clone(),
                ],
            }
        }
        fn manager(&self, child: u8, rights: Vec<AccountRight>) -> PreparedAccountDeviceChange {
            let signed = Signed::sign(self.intent(child, rights), &key(3)).unwrap();
            let qualified = verify_account_enrollment(&self.state(), &signed, NOW).unwrap();
            let proof = sign_device_enrollment_possession(
                &qualified,
                &key(child),
                &Nonce::from_bytes([6; 32]).to_string(),
                NOW as f64,
            )
            .unwrap();
            prepare_account_device_enrollment(
                &qualified,
                &proof,
                &key(3),
                &self.authorization(),
                NOW as f64,
            )
            .unwrap()
        }
        fn accept(&self, prepared: &PreparedAccountDeviceChange) -> VerifiedAccountDeviceChange {
            let request = prepared.request_json().unwrap();
            let challenge =
                issue_account_device_policy_challenge(&request, &key(4), &self.state(), NOW as f64)
                    .unwrap();
            let signed: Signed<PolicyCommit> = parse(&challenge).unwrap();
            assert_eq!(signed.payload.accepted_at_ms, 0);
            let receipts: Vec<_> = signed
                .payload
                .dependencies
                .iter()
                .map(|head| {
                    Signed::sign(
                        FreshnessReceipt {
                            version: AUTH_VERSION,
                            current: head.clone(),
                            challenge_hash: signed.challenge_hash().unwrap(),
                            issued_at_ms: NOW,
                            expires_at_ms: signed.payload.expires_at_ms,
                        },
                        &key(4),
                    )
                    .unwrap()
                })
                .collect();
            let accepted = accept_account_device_policy_change(
                &request,
                &challenge,
                &encode(&receipts).unwrap(),
                &key(4),
                &self.state(),
                NOW as f64,
            )
            .unwrap();
            let commit: Committed<AccountDocument> = parse(&accepted).unwrap();
            assert_eq!(
                signed.challenge_hash().unwrap(),
                commit.commit.challenge_hash().unwrap()
            );
            finish_account_device_change(prepared, &accepted, NOW as f64).unwrap()
        }
        fn passkey_proof(&self, child: u8) -> PasskeyEnrollmentProof {
            let state = self.state();
            let intent = Signed::sign(self.intent(child, vec![]), &key(child)).unwrap();
            let qualified = verify_passkey_enrollment_request(&state, &intent, NOW).unwrap();
            let pending = PendingPasskey::prepare(
                EnrollmentEnvelope {
                    version: AUTH_VERSION,
                    purpose: PasskeyPurpose::EnrollDevice,
                    account: self.account(),
                    device_public_key: PublicKey::of(&key(child)),
                    rights: vec![],
                    target: intent.payload.target.clone(),
                    enrollment_hash: qualified.intent_hash().clone(),
                    expected_heads: intent.payload.expected_heads.clone(),
                    nonce: Nonce::from_bytes([0; 32]),
                    not_before_ms: NOW,
                    expires_at_ms: NOW + 10_000,
                },
                RelyingParty::new("client.example", "https://client.example").unwrap(),
                &[7; 32],
                AuthenticationCeremony::Discoverable,
                vec![],
                NOW,
            )
            .unwrap();
            let certificate = sign_passkey_enrollment_possession(
                &qualified,
                &key(child),
                pending.envelope().nonce.as_str(),
                NOW as f64,
            )
            .unwrap();
            let response = crate::webauthn::tests::assertion(
                &pending,
                crate::webauthn::tests::auth(&pending, 0x0d, 1),
                crate::webauthn::tests::client(&pending, "webauthn.get"),
                Some(URL_SAFE_NO_PAD.encode([7; 32])),
            );
            let record = state
                .authenticator(&state.account(&self.account()).unwrap().authenticators[0])
                .unwrap()
                .clone();
            PasskeyEnrollmentProof {
                intent,
                certificate: parse(&certificate).unwrap(),
                assertion: PasskeyAssertionProof {
                    pending,
                    record,
                    state: AuthenticatorState {
                        sign_count: 0,
                        backup_state: false,
                    },
                    response,
                },
            }
        }
        fn after(&self, result: &VerifiedAccountDeviceChange) -> Self {
            let mut bundle = self.bundle.clone();
            bundle.journal.extend(result.journal_delta.clone());
            Self {
                pins: result.pins.clone(),
                bundle,
            }
        }
    }

    #[test]
    fn native_device_builders_reject_unconfirmed_and_cross_account_handles() {
        let f = Fixture::new();
        let intent = encode(&f.intent(5, vec![])).unwrap();
        let unconfirmed = verify_client_authorization(
            &f.bundle_json(),
            &f.pins_json(),
            f.account().as_str(),
            SpaceId::personal(&f.account()).as_str(),
            &key(3).public_key_hex(),
            NOW as f64,
        )
        .unwrap();
        assert!(sign_device_enrollment_intent(&intent, &key(3), &unconfirmed, NOW as f64).is_err());
        let authorization = f.authorization();
        assert!(
            sign_device_enrollment_intent(&intent, &key(3), &authorization, NOW as f64).is_ok()
        );
        let mut other = f.intent(5, vec![]);
        other.account = AccountId::from_root(&PublicKey::of(&key(99)));
        assert!(
            sign_device_enrollment_intent(
                &encode(&other).unwrap(),
                &key(3),
                &authorization,
                NOW as f64
            )
            .is_err()
        );
        let prepared = f.manager(5, vec![]);
        let result = f.accept(&prepared);
        let next = result.authorization().unwrap();
        assert_eq!(
            next.scope.device_id,
            DeviceId::from_key(&PublicKey::of(&key(3)))
        );
        assert_ne!(
            next.scope.device_id,
            result.scope.as_ref().unwrap().device_id
        );
        assert!(require_confirmed_client_policy(&next).is_ok());
        assert_eq!(
            parse::<Vec<JournalEntry>>(&result.journal_delta_json().unwrap())
                .unwrap()
                .len(),
            2
        );
    }

    #[test]
    fn default_join_grants_no_management_rights_and_replays_after_commit() {
        let f = Fixture::new();
        let p = f.manager(5, vec![]);
        let result = f.accept(&p);
        let current = f.after(&result);
        let state = current.state();
        let account = state.account(&f.account()).unwrap();
        assert!(
            !account
                .enrollment
                .iter()
                .any(|a| a.device == DeviceId::from_key(&PublicKey::of(&key(5))))
        );
        assert_eq!(
            result.scope.as_ref().unwrap().device_id,
            DeviceId::from_key(&PublicKey::of(&key(5)))
        );
        let certificate = state
            .find_device(&DeviceId::from_key(&PublicKey::of(&key(5))))
            .unwrap();
        assert_eq!(
            certificate.payload.uses,
            [DeviceUse::Noise, DeviceUse::AccountInvocation]
        );
        assert!(
            verify_account_enrollment(
                &state,
                &Signed::sign(current.intent(6, vec![]), &key(5)).unwrap(),
                NOW
            )
            .is_err()
        );
    }

    #[test]
    fn public_offer_transfers_only_account_proof_and_constructs_the_bound_intent() {
        let f = Fixture::new();
        let json = create_device_enrollment_offer(
            &key(3),
            f.account().as_str(),
            "https://node.example",
            &key(4).public_key_hex(),
            "[]",
            END as f64,
            &f.authorization(),
            NOW as f64,
        )
        .unwrap();
        let signed: Signed<EnrollmentOffer> = parse(&json).unwrap();
        let mut changed_signature = signed.clone();
        changed_signature.signature = Signature::parse(&"00".repeat(64)).unwrap();
        assert_ne!(
            enrollment_offer_hash(&signed).unwrap(),
            enrollment_offer_hash(&changed_signature).unwrap()
        );
        assert!(verify_enrollment_offer_current(&f.state(), &changed_signature, NOW).is_err());
        assert!(!json.contains("manager_journal"));
        assert!(!json.contains("secret_hex"));
        assert!(!json.contains("grant_leaf"));
        assert_eq!(signed.payload.pins.len(), 2);
        let verified = f.import_offer(&json, &key(5));
        let intent_json =
            create_device_enrollment_intent_from_offer(&verified, &key(5), NOW as f64).unwrap();
        let intent: DeviceEnrollmentIntent = parse(&intent_json).unwrap();
        assert_eq!(intent, f.intent(5, vec![]));
        assert!(
            sign_device_enrollment_intent(&intent_json, &key(3), &f.authorization(), NOW as f64)
                .is_ok()
        );
        assert!(
            create_device_enrollment_intent_from_offer(
                &verified,
                &key(5),
                verified.expires_at_ms()
            )
            .is_err()
        );
        assert!(
            create_device_enrollment_intent_from_offer(&verified, &key(1), NOW as f64).is_err()
        );
        assert!(
            create_device_enrollment_offer(
                &key(1),
                f.account().as_str(),
                "https://node.example",
                &key(4).public_key_hex(),
                "[]",
                END as f64,
                &f.authorization(),
                NOW as f64
            )
            .is_err()
        );
    }

    #[test]
    fn offer_rejects_changed_pins_target_expiry_and_nonmanager_signatures() {
        let f = Fixture::new();
        let json = create_device_enrollment_offer(
            &key(3),
            f.account().as_str(),
            "https://node.example",
            &key(4).public_key_hex(),
            "[]",
            END as f64,
            &f.authorization(),
            NOW as f64,
        )
        .unwrap();
        let signed: Signed<EnrollmentOffer> = parse(&json).unwrap();
        for field in 0..5 {
            let mut offer = signed.payload.clone();
            match field {
                0 => {
                    offer
                        .pins
                        .iter_mut()
                        .find(|pin| matches!(pin.domain, PolicyDomain::Space(_)))
                        .unwrap()
                        .genesis_root = PublicKey::of(&key(19))
                }
                1 => offer.entry.node_public_key = PublicKey::of(&key(19)),
                2 => offer.certificate_expires_at_ms = 1_000_001,
                3 => offer.expires_at_ms = NOW + INVOCATION_MAX_AGE_MS + 1,
                _ => offer.entry.endpoint = "https://user:password@node.example".to_owned(),
            }
            assert!(
                verify_enrollment_offer_current(
                    &f.state(),
                    &Signed::sign(offer, &key(3)).unwrap(),
                    NOW
                )
                .is_err()
            );
        }
        assert!(
            verify_enrollment_offer_current(
                &f.state(),
                &Signed::sign(signed.payload, &key(2)).unwrap(),
                NOW
            )
            .is_err()
        );
    }

    #[test]
    fn public_offer_roundtrips_large_valid_history_under_its_own_bound() {
        let mut f = Fixture::new();
        let mut state = f.state();
        for _ in 0..130 {
            let document =
                Signed::sign(next_document(&state, &f.account()).unwrap(), &key(1)).unwrap();
            let head = state.install_account(document.clone()).unwrap();
            f.bundle.journal.push(JournalEntry::Account(document));
            let pin = f
                .pins
                .iter_mut()
                .find(|pin| pin.domain == head.domain)
                .unwrap();
            pin.minimum_head = Some(head);
        }
        let json = create_device_enrollment_offer(
            &key(3),
            f.account().as_str(),
            "https://node.example",
            &key(4).public_key_hex(),
            "[]",
            END as f64,
            &f.authorization(),
            NOW as f64,
        )
        .unwrap();
        assert!(f.bundle_json().len() > MAX_PROOF_BYTES);
        assert!(json.len() < MAX_ENROLLMENT_OFFER_BYTES);
        assert!(decode_signed::<EnrollmentOffer>(json.as_bytes()).is_ok());
        let verified = f.import_offer(&json, &key(5));
        assert_eq!(
            verified.offer_hash().unwrap(),
            enrollment_offer_hash(&decode_enrollment_offer(json.as_bytes()).unwrap())
                .unwrap()
                .to_string()
        );
        assert!(
            verify_device_enrollment_offer(&" ".repeat(MAX_ENROLLMENT_OFFER_BYTES + 1), NOW as f64)
                .is_err()
        );
        if let Ok(path) = std::env::var("VIBEFACE_ENROLLMENT_VECTORS") {
            let output = std::path::Path::new(&path).with_file_name("large-offer.json");
            std::fs::write(
                output,
                serde_json::to_vec(&serde_json::json!({
                    "offer": json, "hash": verified.offer_hash().unwrap()
                }))
                .unwrap(),
            )
            .unwrap();
        }
    }

    #[test]
    fn offer_approval_checks_the_exact_intent_without_a_business_bundle() {
        let f = Fixture::new();
        let json = create_device_enrollment_offer(
            &key(3),
            f.account().as_str(),
            "https://node.example",
            &key(4).public_key_hex(),
            "[]",
            END as f64,
            &f.authorization(),
            NOW as f64,
        )
        .unwrap();
        let offer = f.import_offer(&json, &key(5));
        let intent_json =
            create_device_enrollment_intent_from_offer(&offer, &key(5), NOW as f64).unwrap();
        let canonical_json = verify_device_enrollment_intent_for_offer(
            &offer,
            &intent_json,
            &key(5).public_key_hex(),
            NOW as f64,
        )
        .unwrap();
        let intent: DeviceEnrollmentIntent = parse(&canonical_json).unwrap();
        assert_eq!(canonical_json.as_bytes(), canonical(&intent).unwrap());
        let signed = Signed::sign(intent.clone(), &key(3)).unwrap();
        let qualified =
            verify_account_enrollment_from_offer(&encode(&signed).unwrap(), &offer, NOW as f64)
                .unwrap();
        assert_eq!(qualified.intent(), &intent);
        assert_eq!(qualified.intent_hash(), &ObjectHash::body(&intent).unwrap());
        for field in 0..8 {
            let mut changed = intent.clone();
            match field {
                0 => changed.target = DeviceId::from_key(&PublicKey::of(&key(19))),
                1 => changed.requested_rights = vec![AccountRight::EnrollDevice],
                2 => changed.certificate_expires_at_ms = END + 1,
                3 => changed.certificate_not_before_ms = NOW - 1,
                4 => changed.certificate_not_before_ms = NOW + 1,
                5 => changed.account = AccountId::from_root(&PublicKey::of(&key(19))),
                6 => changed.expected_heads[0].sequence += 1,
                _ => changed.new_device_public_key = PublicKey::of(&key(19)),
            }
            assert!(
                verify_device_enrollment_intent_for_offer(
                    &offer,
                    &encode(&changed).unwrap(),
                    &key(5).public_key_hex(),
                    NOW as f64
                )
                .is_err()
            );
            if field != 7 {
                assert!(
                    verify_account_enrollment_from_offer(
                        &encode(&Signed::sign(changed, &key(3)).unwrap()).unwrap(),
                        &offer,
                        NOW as f64
                    )
                    .is_err()
                );
            }
        }
        assert!(
            verify_account_enrollment_from_offer(
                &encode(&Signed::sign(intent.clone(), &key(1)).unwrap()).unwrap(),
                &offer,
                NOW as f64
            )
            .is_err()
        );
        assert!(
            verify_account_enrollment_from_offer(
                &encode(&Signed::sign(intent, &key(5)).unwrap()).unwrap(),
                &offer,
                NOW as f64
            )
            .is_err()
        );
        assert!(
            verify_account_enrollment_from_offer(
                &encode(&signed).unwrap(),
                &offer,
                offer.expires_at_ms()
            )
            .is_err()
        );
    }

    #[test]
    fn manager_approves_the_fixed_offer_using_its_own_confirmed_client() {
        let mut f = Fixture::new();
        let authorization = f.authorization();
        let offer_json = create_device_enrollment_offer(
            &key(3),
            f.account().as_str(),
            "https://node.example",
            &key(4).public_key_hex(),
            "[]",
            END as f64,
            &authorization,
            NOW as f64,
        )
        .unwrap();
        let descriptor = verify_device_enrollment_offer(&offer_json, NOW as f64).unwrap();
        let offer_hash = descriptor.offer_hash().unwrap();
        let intent = f.intent(5, vec![]);
        let intent_json = encode(&intent).unwrap();
        let expected_key = intent.new_device_public_key.to_string();
        let verification = crate::signature_verification::SignatureVerification::new();
        let canonical_json = verification
            .run(|| {
                verify_device_enrollment_intent_for_manager(
                    &authorization,
                    &descriptor,
                    &offer_hash,
                    &intent_json,
                    &expected_key,
                    NOW as f64,
                )
            })
            .unwrap();
        assert!(verification.counts().actual_verifications > 0);
        assert!(verification.counts().actual_verifications <= MAX_PROOF_SIGNATURES);
        assert_eq!(canonical_json.as_bytes(), canonical(&intent).unwrap());
        let signed =
            sign_device_enrollment_intent(&canonical_json, &key(3), &authorization, NOW as f64)
                .unwrap();
        let approved: Signed<DeviceEnrollmentIntent> = parse(&signed).unwrap();
        assert_eq!(approved.issuer, PublicKey::of(&key(3)));
        assert_eq!(approved.payload, intent);
        for field in 0..8 {
            let mut changed = intent.clone();
            match field {
                0 => changed.target = DeviceId::from_key(&PublicKey::of(&key(19))),
                1 => changed.requested_rights = vec![AccountRight::EnrollDevice],
                2 => changed.certificate_expires_at_ms = END + 1,
                3 => changed.certificate_not_before_ms = NOW - 1,
                4 => changed.certificate_not_before_ms = NOW + 1,
                5 => changed.account = AccountId::from_root(&PublicKey::of(&key(19))),
                6 => changed.expected_heads[0].sequence += 1,
                _ => changed.new_device_public_key = PublicKey::of(&key(19)),
            }
            assert!(
                verify_device_enrollment_intent_for_manager(
                    &authorization,
                    &descriptor,
                    &offer_hash,
                    &encode(&changed).unwrap(),
                    &expected_key,
                    NOW as f64
                )
                .is_err()
            );
        }
        assert!(
            verify_device_enrollment_intent_for_manager(
                &authorization,
                &descriptor,
                &ObjectHash::body(&"another scanned offer")
                    .unwrap()
                    .to_string(),
                &intent_json,
                &expected_key,
                NOW as f64
            )
            .is_err()
        );
        assert!(
            verify_device_enrollment_intent_for_manager(
                &authorization,
                &descriptor,
                &offer_hash,
                &intent_json,
                &key(19).public_key_hex(),
                NOW as f64
            )
            .is_err()
        );
        assert!(
            verify_device_enrollment_intent_for_manager(
                &authorization,
                &descriptor,
                &offer_hash,
                &intent_json,
                &expected_key,
                descriptor.expires_at_ms()
            )
            .is_err()
        );
        assert!(
            verify_device_enrollment_intent_for_manager(
                &authorization,
                &descriptor,
                &offer_hash,
                &format!("{}{}", " ".repeat(MAX_PROOF_BYTES), intent_json),
                &expected_key,
                NOW as f64
            )
            .is_err()
        );
        let unconfirmed = verify_client_authorization(
            &f.bundle_json(),
            &f.pins_json(),
            f.account().as_str(),
            SpaceId::personal(&f.account()).as_str(),
            &key(3).public_key_hex(),
            NOW as f64,
        )
        .unwrap();
        assert!(
            verify_device_enrollment_intent_for_manager(
                &unconfirmed,
                &descriptor,
                &offer_hash,
                &intent_json,
                &expected_key,
                NOW as f64
            )
            .is_err()
        );
        let mut wrong_manager: Signed<EnrollmentOffer> = parse(&offer_json).unwrap();
        wrong_manager = Signed::sign(wrong_manager.payload, &key(2)).unwrap();
        let wrong_manager_json = encode(&wrong_manager).unwrap();
        let wrong_descriptor =
            verify_device_enrollment_offer(&wrong_manager_json, NOW as f64).unwrap();
        assert!(
            verify_device_enrollment_intent_for_manager(
                &authorization,
                &wrong_descriptor,
                &wrong_descriptor.offer_hash().unwrap(),
                &intent_json,
                &expected_key,
                NOW as f64
            )
            .is_err()
        );
        let mut state = f.state();
        let mut document = next_document(&state, &f.account()).unwrap();
        document.enrollment.clear();
        let signed = Signed::sign(document, &key(1)).unwrap();
        let head = state.install_account(signed.clone()).unwrap();
        f.bundle.journal.push(JournalEntry::Account(signed));
        f.pins
            .iter_mut()
            .find(|pin| pin.domain == head.domain)
            .unwrap()
            .minimum_head = Some(head.clone());
        let current = f.authorization();
        assert!(
            verify_device_enrollment_intent_for_manager(
                &current,
                &descriptor,
                &offer_hash,
                &intent_json,
                &expected_key,
                NOW as f64
            )
            .is_err()
        );
    }

    #[test]
    fn historical_root_and_recovery_keys_cannot_be_enrolled_as_daily_devices() {
        let f = Fixture::new();
        let mut state = f.state();
        let mut document = next_document(&state, &f.account()).unwrap();
        document.authority_epoch += 1;
        document.sequence = 0;
        document.root_generation += 1;
        document.current_root = PublicKey::of(&key(12));
        document.accept_root(&key(12)).unwrap();
        document.approve_recovery(&key(2)).unwrap();
        state
            .install_account(Signed::sign(document, &key(2)).unwrap())
            .unwrap();
        let mut document = next_document(&state, &f.account()).unwrap();
        document.recovery.generation += 1;
        document.recovery.keys = vec![PublicKey::of(&key(13))];
        document.approve_recovery(&key(2)).unwrap();
        state
            .install_account(Signed::sign(document, &key(12)).unwrap())
            .unwrap();
        for child in [1, 2] {
            let mut intent = f.intent(child, vec![]);
            intent.expected_heads = vec![
                state
                    .head(&PolicyDomain::Account(f.account()))
                    .unwrap()
                    .clone(),
            ];
            assert_eq!(
                verify_account_enrollment(
                    &state,
                    &Signed::sign(intent.clone(), &key(12)).unwrap(),
                    NOW
                )
                .unwrap_err(),
                AuthorizationError::InvalidProof(
                    "enrollment requires an independent new device key"
                )
            );
            assert_eq!(
                verify_passkey_enrollment_request(
                    &state,
                    &Signed::sign(intent, &key(child)).unwrap(),
                    NOW
                )
                .unwrap_err(),
                AuthorizationError::InvalidProof(
                    "enrollment requires an independent new device key"
                )
            );
        }
    }

    #[test]
    fn delegated_manager_is_independent_after_old_enroller_revocation() {
        let f = Fixture::new();
        let manager = f.accept(&f.manager(
            5,
            vec![AccountRight::EnrollDevice, AccountRight::RevokeDevice],
        ));
        let f = f.after(&manager);
        let p = prepare_account_device_revocation(
            f.account().as_str(),
            DeviceId::from_key(&PublicKey::of(&key(3))).as_str(),
            &key(1),
            &f.authorization(),
            NOW as f64,
        )
        .unwrap();
        let result = f.accept(&p);
        let f = f.after(&result);
        let state = f.state();
        let intent = f.intent(6, vec![AccountRight::EnrollDevice]);
        assert!(
            verify_account_enrollment(&state, &Signed::sign(intent.clone(), &key(5)).unwrap(), NOW)
                .is_ok()
        );
        assert!(
            verify_account_enrollment(&state, &Signed::sign(intent, &key(3)).unwrap(), NOW)
                .is_err()
        );
        assert!(
            verify_client_authorization(
                &f.bundle_json(),
                &f.pins_json(),
                f.account().as_str(),
                SpaceId::personal(&f.account()).as_str(),
                &key(5).public_key_hex(),
                NOW as f64
            )
            .is_ok()
        );
    }

    #[test]
    fn manager_cannot_extend_rights_expiry_or_reenroll_an_existing_device() {
        let mut f = Fixture::new();
        let state = f.state();
        let mut d = next_document(&state, &f.account()).unwrap();
        d.enrollment[0].rights = vec![AccountRight::EnrollDevice];
        d.enrollment[0].expires_at_ms = NOW + 100;
        f.bundle
            .journal
            .push(JournalEntry::Account(Signed::sign(d, &key(1)).unwrap()));
        let state = f.state();
        let mut i = f.intent(5, vec![AccountRight::RevokeDevice]);
        i.certificate_expires_at_ms = NOW + 100;
        assert!(
            verify_account_enrollment(&state, &Signed::sign(i.clone(), &key(3)).unwrap(), NOW)
                .is_err()
        );
        i.requested_rights.clear();
        i.certificate_expires_at_ms += 1;
        assert!(
            verify_account_enrollment(&state, &Signed::sign(i.clone(), &key(3)).unwrap(), NOW)
                .is_err()
        );
        i.certificate_expires_at_ms -= 1;
        assert!(
            verify_account_enrollment(
                &state,
                &Signed::sign(i.clone(), &key(3)).unwrap(),
                NOW + 100
            )
            .is_err()
        );
        i.new_device_public_key = PublicKey::of(&key(3));
        assert!(
            verify_account_enrollment(&state, &Signed::sign(i, &key(3)).unwrap(), NOW).is_err()
        );
    }

    #[test]
    fn account_delta_rejects_empty_existing_device_delegation_and_root_changes() {
        let f = Fixture::new();
        let state = f.state();
        let previous = state.account(&f.account()).unwrap();
        for change in 0..3 {
            let mut d = next_document(&state, &f.account()).unwrap();
            match change {
                0 => {}
                1 => d.enrollment[0].rights.clear(),
                2 => d.current_root = PublicKey::of(&key(9)),
                _ => unreachable!(),
            }
            assert!(
                state
                    .authorize_account_delta(
                        &Signed::sign(d, &key(3)).unwrap(),
                        previous,
                        NOW,
                        &mut Budget::default()
                    )
                    .is_err()
            );
        }
        let p = f.manager(5, vec![AccountRight::EnrollDevice]);
        let mut state = f.state();
        if let AccountDeviceChange::Enrollment { certificate, .. } = &p.request.change {
            state.install_device(certificate.clone(), NOW).unwrap();
        }
        let mut d = p.request.proposal.payload.clone();
        d.enrollment
            .last_mut()
            .unwrap()
            .rights
            .push(AccountRight::ManageAuthenticators);
        d.enrollment.last_mut().unwrap().expires_at_ms = 1_000_001;
        assert!(
            state
                .authorize_account_delta(
                    &Signed::sign(d, &key(3)).unwrap(),
                    previous,
                    NOW,
                    &mut Budget::default()
                )
                .is_err()
        );
    }

    #[test]
    fn passkey_only_client_enrollment_needs_real_uv_and_replays_exact_delta() {
        let f = Fixture::new();
        let proof = f.passkey_proof(5);
        let mut state = f.state();
        assert!(verify_account_enrollment(&state, &proof.intent, NOW).is_err());
        assert!(
            state
                .install_device(proof.certificate.clone(), NOW)
                .is_err()
        );
        let verified = verify_passkey_device_enrollment(&state, &proof, NOW).unwrap();
        assert_eq!(verified.verified_assertion().previous_state().sign_count, 0);
        assert_eq!(verified.verified_assertion().sign_count(), 1);
        let p = prepare_passkey_device_enrollment(
            &encode(&proof.intent).unwrap(),
            &encode(&proof.certificate).unwrap(),
            &encode(&proof.assertion).unwrap(),
            &key(5),
            &f.authorization(),
            NOW as f64,
        )
        .unwrap();
        let mut budget = Budget::default();
        authorize_passkey_delta(
            &state,
            &p.request.proposal,
            state.account(&f.account()).unwrap(),
            NOW,
            &mut budget,
        )
        .unwrap();
        assert_eq!(budget.external_signatures.len(), 1);
        assert_eq!(budget.signatures.len(), 4);
        let result = f.accept(&p);
        assert_eq!(
            f.after(&result).bundle.journal.len(),
            f.bundle.journal.len() + 1
        );
        let state = f.after(&result).state();
        let account = state.account(&f.account()).unwrap();
        assert!(
            account
                .enrollment
                .iter()
                .all(|e| e.device != proof.certificate.payload.device)
        );
        assert!(account.device_enrollment.is_some());
        assert_eq!(
            state
                .find_device(&proof.certificate.payload.device)
                .unwrap(),
            &proof.certificate
        );
        assert_eq!(
            result.scope.unwrap().device_id,
            proof.certificate.payload.device
        );
    }

    #[test]
    fn completed_device_receipts_survive_pending_expiry_without_new_authority() {
        let f = Fixture::new();
        let proof = f.passkey_proof(6);
        let passkey = prepare_passkey_device_enrollment(
            &encode(&proof.intent).unwrap(),
            &encode(&proof.certificate).unwrap(),
            &encode(&proof.assertion).unwrap(),
            &key(6),
            &f.authorization(),
            NOW as f64,
        )
        .unwrap();
        for prepared in [f.manager(5, vec![]), passkey] {
            let result = f.accept(&prepared);
            let Some(JournalEntry::AccountCommit(committed)) = result.journal_delta.last() else {
                panic!("missing committed result")
            };
            let receipt = encode(committed).unwrap();
            let delayed =
                finish_account_device_change(&prepared, &receipt, (NOW + 200_000) as f64).unwrap();
            assert_eq!(delayed.pins_json().unwrap(), result.pins_json().unwrap());
            let expired =
                finish_account_device_change(&prepared, &receipt, (END + 1) as f64).unwrap();
            assert_eq!(expired.pins_json().unwrap(), result.pins_json().unwrap());
            assert_eq!(expired.scope_json().unwrap(), "null");
            assert!(finish_account_device_change(&prepared, &receipt, (NOW - 1) as f64).is_err());
        }
    }

    #[test]
    fn passkey_offer_finish_returns_only_delta_without_inventing_client_authority() {
        use crate::authorization::passkey_node::*;
        let f = Fixture::new();
        let offer_json = create_device_enrollment_offer(
            &key(3),
            f.account().as_str(),
            "https://node.example",
            &key(4).public_key_hex(),
            "[]",
            END as f64,
            &f.authorization(),
            NOW as f64,
        )
        .unwrap();
        let (offer, offer_policy) = f.import_offer_with_evidence(&offer_json, &key(5));
        let proof = f.passkey_proof(5);
        let signed = sign_passkey_enrollment_intent_from_offer(
            &encode(&proof.intent.payload).unwrap(),
            &key(5),
            &offer,
            NOW as f64,
        )
        .unwrap();
        assert_eq!(
            parse::<Signed<DeviceEnrollmentIntent>>(&signed).unwrap(),
            proof.intent
        );
        let qualified =
            verify_passkey_enrollment_request_from_offer(&signed, &offer, NOW as f64).unwrap();
        let signed_pending = Signed::sign(
            NodePasskeyPending {
                version: AUTH_VERSION,
                pending: proof.assertion.pending.clone(),
                issued_at_ms: NOW,
                expires_at_ms: NOW + 10_000,
            },
            &key(4),
        )
        .unwrap();
        let pending = verify_node_passkey_pending(
            &encode(&signed_pending).unwrap(),
            &key(4).public_key_hex(),
            &signed,
            "client.example",
            "https://client.example",
            NOW as f64,
        )
        .unwrap();
        let signed_state = Signed::sign(
            NodePasskeyState {
                version: AUTH_VERSION,
                pending: proof.assertion.pending.clone(),
                credential_id: proof.assertion.record.payload.credential_id.clone(),
                record_hash: proof.assertion.record.hash().unwrap(),
                state: proof.assertion.state,
                response_hash: ObjectHash::body(&proof.assertion.response).unwrap(),
                issued_at_ms: NOW,
                expires_at_ms: NOW + 10_000,
            },
            &key(4),
        )
        .unwrap();
        let node_state = verify_node_passkey_state(
            &encode(&signed_state).unwrap(),
            &pending,
            &encode(&proof.assertion).unwrap(),
            &encode(&proof.assertion.response).unwrap(),
            NOW as f64,
        )
        .unwrap();
        let envelope_hash = pending.envelope_hash().unwrap();
        let verify_assertion =
            |proof: &PasskeyAssertionProof, hash: &str, rp: &str, origin: &str, now: f64| {
                verify_passkey_assertion_from_enrollment_offer(
                    &encode(proof).unwrap(),
                    &node_state,
                    &offer,
                    hash,
                    rp,
                    origin,
                    now,
                )
            };
        let actual = crate::signature_verification::SignatureVerification::new();
        let assertion = actual
            .run(|| {
                verify_assertion(
                    &proof.assertion,
                    &envelope_hash,
                    "client.example",
                    "https://client.example",
                    NOW as f64,
                )
            })
            .unwrap();
        assert!(actual.counts().actual_verifications <= MAX_PROOF_SIGNATURES);
        assert_eq!(assertion.sign_count(), 1);
        assert_eq!(assertion.previous_state().sign_count, 0);
        assert_eq!(
            assertion.record_hash(),
            &proof.assertion.record.hash().unwrap()
        );
        assert!(!assertion.backup_state());
        let mut changed = proof.assertion.clone();
        changed.response.client_extension_results = serde_json::json!({"changed":true});
        assert!(
            verify_assertion(
                &changed,
                &envelope_hash,
                "client.example",
                "https://client.example",
                NOW as f64
            )
            .is_err()
        );
        changed = proof.assertion.clone();
        changed.state.sign_count = 1;
        assert!(
            verify_assertion(
                &changed,
                &envelope_hash,
                "client.example",
                "https://client.example",
                NOW as f64
            )
            .is_err()
        );
        for (hash, rp, origin, now) in [
            ("wrong", "client.example", "https://client.example", NOW),
            (
                envelope_hash.as_str(),
                "other.example",
                "https://client.example",
                NOW,
            ),
            (
                envelope_hash.as_str(),
                "client.example",
                "https://other.example",
                NOW,
            ),
            (
                envelope_hash.as_str(),
                "client.example",
                "https://client.example",
                NOW + 10_000,
            ),
        ] {
            assert!(verify_assertion(&proof.assertion, hash, rp, origin, now as f64).is_err());
        }
        #[cfg(unix)]
        if let Ok(path) = std::env::var("VIBEFACE_OFFER_ASSERTION_VECTOR") {
            use std::io::Write;
            use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .mode(0o600)
                .open(path)
                .unwrap();
            file.set_permissions(std::fs::Permissions::from_mode(0o600))
                .unwrap();
            file.write_all(&serde_json::to_vec(&serde_json::json!({
                "now":NOW,"reader_fixture_byte":5,"node_public_key":key(4).public_key_hex(),
                "offer":parse::<serde_json::Value>(&offer_json).unwrap(),"offer_policy":offer_policy,
                "intent":proof.intent,"signed_pending":signed_pending,"signed_state":signed_state,
                "proof":proof.assertion,"browser_response":proof.assertion.response,"envelope_hash":envelope_hash,
                "rp_id":"client.example","origin":"https://client.example","actual":actual.counts(),
            })).unwrap()).unwrap();
        }
        let certificate = sign_passkey_enrollment_possession(
            &qualified,
            &key(5),
            proof.certificate.payload.nonce.as_str(),
            NOW as f64,
        )
        .unwrap();
        let prepared = prepare_passkey_device_enrollment_from_offer(
            &signed,
            &certificate,
            &encode(&proof.assertion).unwrap(),
            &key(5),
            &offer,
            NOW as f64,
        )
        .unwrap();
        let result = f.accept(&prepared);
        assert!(result.authorization().is_err());
        assert_eq!(result.scope_json().unwrap(), "null");
        let journal_json = result.journal_delta_json().unwrap();
        assert!(!journal_json.contains("grant_leaf"));
        let journal: Vec<JournalEntry> = parse(&journal_json).unwrap();
        assert_eq!(journal.len(), 1);
        assert!(journal.iter().all(|entry| matches!(
            entry,
            JournalEntry::AccountGenesis(_)
                | JournalEntry::Account(_)
                | JournalEntry::AccountCommit(_)
                | JournalEntry::Device(_)
                | JournalEntry::Authenticator(_)
        )));
        assert!(
            f.after(&result)
                .state()
                .find_device(&proof.certificate.payload.device)
                .is_ok()
        );
        let full_prepared = prepare_passkey_device_enrollment(
            &signed,
            &certificate,
            &encode(&proof.assertion).unwrap(),
            &key(5),
            &f.authorization(),
            NOW as f64,
        )
        .unwrap();
        let full_result = f.accept(&full_prepared);
        assert!(
            verify_client_authorization(
                &f.after(&full_result).bundle_json(),
                &result.pins_json().unwrap(),
                f.account().as_str(),
                SpaceId::personal(&f.account()).as_str(),
                &key(5).public_key_hex(),
                NOW as f64
            )
            .is_ok()
        );
        let mut invalid = proof.assertion;
        invalid.state.sign_count = 2;
        assert!(
            prepare_passkey_device_enrollment_from_offer(
                &signed,
                &certificate,
                &encode(&invalid).unwrap(),
                &key(5),
                &offer,
                NOW as f64
            )
            .is_err()
        );
    }

    #[test]
    fn native_wasm_device_change_vectors() {
        let f = Fixture::new();
        let manager = f.manager(5, vec![]);
        let joined = f.accept(&manager);
        let joined_fixture = f.after(&joined);
        let revoke = prepare_account_device_revocation(
            f.account().as_str(),
            DeviceId::from_key(&PublicKey::of(&key(5))).as_str(),
            &key(1),
            &joined_fixture.authorization(),
            NOW as f64,
        )
        .unwrap();
        let revoked = joined_fixture.accept(&revoke);
        let passkey_proof = f.passkey_proof(7);
        let passkey = prepare_passkey_device_enrollment(
            &encode(&passkey_proof.intent).unwrap(),
            &encode(&passkey_proof.certificate).unwrap(),
            &encode(&passkey_proof.assertion).unwrap(),
            &key(7),
            &f.authorization(),
            NOW as f64,
        )
        .unwrap();
        let passkey_result = f.accept(&passkey);
        let committed = |result: &VerifiedAccountDeviceChange| {
            let Some(JournalEntry::AccountCommit(value)) = result.journal_delta.last() else {
                panic!("missing committed device change")
            };
            value.clone()
        };
        let offer = create_device_enrollment_offer(
            &key(3),
            f.account().as_str(),
            "https://node.example",
            &key(4).public_key_hex(),
            "[]",
            END as f64,
            &f.authorization(),
            NOW as f64,
        )
        .unwrap();
        let verified_offer = f.import_offer(&offer, &key(5));
        assert_eq!(
            verified_offer.offer_hash().unwrap(),
            ObjectHash::body(&parse::<Signed<EnrollmentOffer>>(&offer).unwrap())
                .unwrap()
                .to_string()
        );
        let AccountDeviceChange::Enrollment { intent, .. } = &manager.request.change else {
            panic!("missing manager intent")
        };
        let manager_intent_hash = ObjectHash::body(&intent.payload).unwrap();
        let vector = serde_json::json!({
            "now": NOW, "end": END, "account": f.account(), "pins": f.pins, "bundle": f.bundle,
            "offer": parse::<Signed<EnrollmentOffer>>(&offer).unwrap(), "offer_hash": verified_offer.offer_hash().unwrap(),
            "manager": {"intent_hash": manager_intent_hash, "request": manager.request, "committed": committed(&joined), "bundle": f.after(&joined).bundle, "pins": joined.pins, "scope": joined.scope},
            "revoke": {"request": revoke.request, "committed": committed(&revoked), "bundle": joined_fixture.after(&revoked).bundle, "pins": revoked.pins, "scope": revoked.scope},
            "passkey": {"proof": passkey_proof, "request": passkey.request, "committed": committed(&passkey_result), "bundle": f.after(&passkey_result).bundle, "pins": passkey_result.pins, "scope": passkey_result.scope}
        });
        if let Ok(path) = std::env::var("VIBEFACE_ENROLLMENT_VECTORS") {
            std::fs::write(path, serde_json::to_vec(&vector).unwrap()).unwrap();
        }
    }

    #[test]
    fn passkey_enrollment_replay_preserves_the_verified_counter_floor() {
        let f = Fixture::new();
        let proof = f.passkey_proof(5);
        let prepared = prepare_passkey_device_enrollment(
            &encode(&proof.intent).unwrap(),
            &encode(&proof.certificate).unwrap(),
            &encode(&proof.assertion).unwrap(),
            &key(5),
            &f.authorization(),
            NOW as f64,
        )
        .unwrap();
        let f = f.after(&f.accept(&prepared));
        let state = f.state();
        let mut proof = f.passkey_proof(6);
        assert_eq!(
            state
                .verify_authenticator(&proof.assertion.record, NOW)
                .unwrap()
                .state_floor()
                .sign_count,
            1,
        );
        assert!(verify_passkey_device_enrollment(&state, &proof, NOW).is_err());
        proof.assertion.state.sign_count = 1;
        assert!(verify_passkey_device_enrollment(&state, &proof, NOW).is_err());
        let pending = &proof.assertion.pending;
        proof.assertion.response = crate::webauthn::tests::assertion(
            pending,
            crate::webauthn::tests::auth(pending, 0x0d, 2),
            crate::webauthn::tests::client(pending, "webauthn.get"),
            Some(URL_SAFE_NO_PAD.encode([7; 32])),
        );
        verify_passkey_device_enrollment(&state, &proof, NOW).unwrap();
        let prepared = prepare_passkey_device_enrollment(
            &encode(&proof.intent).unwrap(),
            &encode(&proof.certificate).unwrap(),
            &encode(&proof.assertion).unwrap(),
            &key(6),
            &f.authorization(),
            NOW as f64,
        )
        .unwrap();
        let result = f.accept(&prepared);
        let replayed = f.after(&result).state();
        assert_eq!(
            replayed
                .verify_authenticator(&proof.assertion.record, NOW)
                .unwrap()
                .state_floor()
                .sign_count,
            2,
        );
    }

    #[test]
    fn passkey_rejects_wrong_intent_scope_nonce_uv_and_unrelated_account_change() {
        let f = Fixture::new();
        let original = f.passkey_proof(5);
        let state = f.state();
        for change in 0..5 {
            let mut proof = original.clone();
            match change {
                0 => {
                    proof.intent.payload.target = DeviceId::from_key(&PublicKey::of(&key(7)));
                    proof.intent = Signed::sign(proof.intent.payload, &key(5)).unwrap();
                }
                1 => {
                    proof
                        .intent
                        .payload
                        .requested_rights
                        .push(AccountRight::EnrollDevice);
                    proof.intent = Signed::sign(proof.intent.payload, &key(5)).unwrap();
                }
                2 => {
                    proof.certificate.payload.nonce = Nonce::from_bytes([1; 32]);
                    proof.certificate.payload.prove_possession(&key(5)).unwrap();
                    proof.certificate = Signed::sign(proof.certificate.payload, &key(5)).unwrap();
                }
                3 => {
                    let p = &proof.assertion.pending;
                    proof.assertion.response = crate::webauthn::tests::assertion(
                        p,
                        crate::webauthn::tests::auth(p, 0x09, 1),
                        crate::webauthn::tests::client(p, "webauthn.get"),
                        Some(URL_SAFE_NO_PAD.encode([7; 32])),
                    );
                }
                4 => {
                    proof.certificate.payload.uses.push(DeviceUse::EnrollDevice);
                    proof.certificate.payload.prove_possession(&key(5)).unwrap();
                    proof.certificate = Signed::sign(proof.certificate.payload, &key(5)).unwrap();
                }
                _ => unreachable!(),
            }
            assert!(verify_passkey_device_enrollment(&state, &proof, NOW).is_err());
        }
        let mut d = next_document(&state, &f.account()).unwrap();
        d.devices.push(original.certificate.hash().unwrap());
        d.devices.sort();
        d.device_enrollment = Some(original);
        d.recovery.threshold = 0;
        assert!(
            state
                .authorize_account_delta(
                    &Signed::sign(d, &key(5)).unwrap(),
                    state.account(&f.account()).unwrap(),
                    NOW,
                    &mut Budget::default()
                )
                .is_err()
        );
    }

    #[test]
    fn eligibility_is_short_lived_and_authenticator_requester_is_current_device() {
        let f = Fixture::new();
        let state = f.state();
        let intent = Signed::sign(f.intent(5, vec![]), &key(3)).unwrap();
        let qualified = verify_account_enrollment(&state, &intent, NOW).unwrap();
        assert_eq!(
            qualified.intent_hash(),
            &ObjectHash::body(&intent.payload).unwrap()
        );
        assert_eq!(qualified.expires_at_ms(), NOW + INVOCATION_MAX_AGE_MS);
        let mut auth = AuthenticatorEnrollmentIntent {
            account: f.account(),
            requesting_device_public_key: PublicKey::of(&key(3)),
            target: intent.payload.target.clone(),
            credential_not_before_ms: NOW,
            credential_expires_at_ms: 2_000_000,
            expected_heads: intent.payload.expected_heads,
        };
        let verified = verify_authenticator_enrollment(
            &state,
            &Signed::sign(auth.clone(), &key(3)).unwrap(),
            NOW,
        )
        .unwrap();
        assert_eq!(verified.expires_at_ms(), NOW + INVOCATION_MAX_AGE_MS);
        auth.requesting_device_public_key = PublicKey::of(&key(4));
        assert!(
            verify_authenticator_enrollment(&state, &Signed::sign(auth, &key(3)).unwrap(), NOW)
                .is_err()
        );
    }
}
