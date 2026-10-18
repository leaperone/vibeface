//! The first passkey uses a local root credential and confirmed current policy.

use super::*;
use crate::authorization::enrollment::{
    AuthenticatorEnrollmentIntent, verify_authenticator_enrollment,
};
use crate::authorization::{
    client_policy_import::require_confirmed_client_policy,
    execution_handoff::ExecutionProofUsage,
    policy_import::VerifiedPolicySnapshot,
    policy_proof::{CachedPolicyProof, policy_entry_usage_with_cache},
};
use crate::signature_verification::{ScopedSignatureVerification, SignatureVerification};
use crate::webauthn::{AuthenticatorState, PasskeyPurpose, VerifiedPasskeyRegistration};

#[derive(Clone, Debug, Serialize)]
struct FirstRegistrationRequest {
    intent: Signed<AuthenticatorEnrollmentIntent>,
    record: Signed<AuthenticatorRecord>,
    proposal: Signed<AccountDocument>,
}

#[derive(Clone, Debug)]
#[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen)]
pub struct PreparedFirstAuthenticatorRegistration {
    request: FirstRegistrationRequest,
    source: VerifiedPolicySnapshot,
    usage: ExecutionProofUsage,
}

#[derive(Clone, Debug)]
#[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen)]
pub struct VerifiedFirstAuthenticatorRegistration {
    snapshot: VerifiedPolicySnapshot,
    usage: ExecutionProofUsage,
}

struct FirstRegistrationContext {
    root: Identity,
    source: VerifiedPolicySnapshot,
    current: VerifiedClientAuthorization,
    usage: ExecutionProofUsage,
}

fn context(
    material_json: &str,
    authorization: &VerifiedClientAuthorization,
    requester: &PublicKey,
    now: u64,
) -> Result<FirstRegistrationContext> {
    let source = require_confirmed_client_policy(authorization)?;
    let material = read_material(material_json)?;
    validate_material(&material)?;
    let root = Identity::from_secret_hex(&material.root_secret_hex)
        .map_err(|_| AuthorizationError::InvalidProof("invalid first registration root"))?;
    let state = source.state();
    let account_id = &material.scope.account_id;
    let account = state
        .account(account_id)
        .ok_or(AuthorizationError::Revoked)?;
    require(
        authorization.scope.account_id == *account_id
            && authorization.scope.space_id == SpaceId::personal(account_id)
            && authorization.scope.device_id == DeviceId::from_key(requester)
            && account.current_root == PublicKey::of(&root)
            && account.authority.public_key == material.entry.node_public_key
            && authorization.audience_public_key == account.authority.public_key
            && authorization.resources.len() == 1
            && authorization.resources[0].payload.host == account.authority.service
            && material.pins.iter().all(|saved| {
                source
                    .pins()
                    .iter()
                    .any(|pin| pin.domain == saved.domain && pin.genesis_root == saved.genesis_root)
                    && saved
                        .minimum_head
                        .as_ref()
                        .is_none_or(|head| state.roots.contains_key(head))
            }),
        "first registration changed its saved root, account client or personal authority",
    )?;
    let selected = ResourceRef {
        space: authorization.scope.space_id.clone(),
        resource: authorization.resources[0].payload.resource.clone(),
    };
    let current = verify_client_policy_state_selected(
        state.clone(),
        &ClientAuthorizationBundle {
            journal: Vec::new(),
            grant_leaf: authorization.grant_leaf.clone(),
        },
        source.pins(),
        account_id.clone(),
        selected.space.clone(),
        requester.clone(),
        Some(&selected),
        now,
    )?;
    require(
        current.heads == authorization.heads
            && current.audience == authorization.audience
            && current.audience_public_key == authorization.audience_public_key,
        "first registration requires explicit current policy synchronization",
    )?;
    state.ensure_personal_current(
        state
            .space(&selected.space)
            .ok_or(AuthorizationError::Revoked)?,
    )?;
    require(
        !source.has_registered_authenticator(&account.account, account.root_generation),
        "recovery material is only used for the first authenticator of this root generation",
    )?;
    let mut usage = recovery_material_usage(&material)?;
    usage.record_budget(&current.budget)?;
    usage.record_chain_depth(state.grant_chain(Some(&current.grant_leaf))?.len());
    usage.enforce_limits()?;
    Ok(FirstRegistrationContext {
        root,
        source: source.clone(),
        current,
        usage,
    })
}

fn finish_usage(usage: &mut ExecutionProofUsage) -> Result<()> {
    if let Some(counts) = SignatureVerification::current_counts() {
        usage.record_actual_signature_counts(counts);
    }
    usage.enforce_limits()
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen(js_name = buildFirstAuthenticatorEnrollmentIntent))]
pub fn build_first_authenticator_enrollment_intent(
    material_json: &str,
    authorization: &VerifiedClientAuthorization,
    requesting_client: &Identity,
    now_ms: f64,
    credential_expires_at_ms: f64,
) -> crate::Result<String> {
    let _scope = ScopedSignatureVerification::new();
    let build = || -> Result<Signed<AuthenticatorEnrollmentIntent>> {
        let now = timestamp(now_ms)?;
        let expires = timestamp(credential_expires_at_ms)?;
        require(
            material_json.len().saturating_add(
                canonical(&(PublicKey::of(requesting_client), now, expires))?.len(),
            ) <= MAX_PROOF_BYTES,
            "first registration intent carrier too large",
        )?;
        let mut verified = context(
            material_json,
            authorization,
            &PublicKey::of(requesting_client),
            now,
        )?;
        let state = verified.source.state();
        let certificate =
            state.find_device(&DeviceId::from_key(&PublicKey::of(requesting_client)))?;
        let Actor::Account(account) = &certificate.payload.subject else {
            return Err(AuthorizationError::InvalidProof(
                "first registration requires an account client",
            ));
        };
        require(
            account == &verified.current.scope.account_id
                && state
                    .account(account)
                    .is_some_and(|doc| doc.current_root == PublicKey::of(&verified.root)),
            "first registration client belongs to another account",
        )?;
        let document = state.account(account).ok_or(AuthorizationError::Revoked)?;
        let signed = Signed::sign(
            AuthenticatorEnrollmentIntent {
                account: account.clone(),
                requesting_device_public_key: PublicKey::of(requesting_client),
                target: document.authority.device.clone(),
                credential_not_before_ms: now,
                credential_expires_at_ms: expires,
                expected_heads: vec![
                    state
                        .head(&PolicyDomain::Account(account.clone()))
                        .ok_or(AuthorizationError::Revoked)?
                        .clone(),
                ],
            },
            &verified.root,
        )?;
        verify_authenticator_enrollment(state, &signed, now)?;
        verified.usage.record_signed(&signed)?;
        finish_usage(&mut verified.usage)?;
        Ok(signed)
    };
    encode(&build().map_err(|e| e.to_string())?)
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen(js_name = prepareFirstAuthenticatorRegistration))]
pub fn prepare_first_authenticator_registration(
    material_json: &str,
    authorization: &VerifiedClientAuthorization,
    signed_intent_json: &str,
    registration: &VerifiedPasskeyRegistration,
    now_ms: f64,
) -> crate::Result<PreparedFirstAuthenticatorRegistration> {
    let _scope = ScopedSignatureVerification::new();
    let prepare = || -> Result<PreparedFirstAuthenticatorRegistration> {
        let now = timestamp(now_ms)?;
        require(
            material_json.len().saturating_add(signed_intent_json.len()) <= MAX_PROOF_BYTES,
            "first registration prepare carrier too large",
        )?;
        let mut verified = context(
            material_json,
            authorization,
            &registration.envelope().device_public_key,
            now,
        )?;
        let mut state = verified.source.state().clone();
        let cached = CachedPolicyProof::from_verified_snapshot(&verified.source);
        let intent: Signed<AuthenticatorEnrollmentIntent> =
            decode_signed(signed_intent_json.as_bytes())?;
        let token = verify_authenticator_enrollment(&state, &intent, now)?;
        require(
            intent.issuer == PublicKey::of(&verified.root),
            "first authenticator intent is not root approved",
        )?;
        let i = &intent.payload;
        require(
            i.account == verified.current.scope.account_id
                && DeviceId::from_key(&i.requesting_device_public_key)
                    == authorization.scope.device_id
                && state
                    .account(&i.account)
                    .is_some_and(|account| account.authority.device == i.target),
            "first registration changed the saved account or current authority",
        )?;
        let envelope = registration.envelope();
        validate_window(envelope.not_before_ms, envelope.expires_at_ms, now)?;
        require(
            envelope.purpose == PasskeyPurpose::RegisterAuthenticator
                && envelope.account == i.account
                && envelope.device_public_key == i.requesting_device_public_key
                && envelope.rights == [AccountRight::ManageAuthenticators]
                && envelope.target == i.target
                && envelope.enrollment_hash == *token.intent_hash()
                && envelope.expected_heads == i.expected_heads
                && envelope.not_before_ms >= i.credential_not_before_ms
                && envelope.expires_at_ms <= token.expires_at_ms(),
            "first authenticator registration changed the approved intent",
        )?;
        let head = state
            .head(&PolicyDomain::Account(i.account.clone()))
            .ok_or(AuthorizationError::Revoked)?
            .clone();
        let record = Signed::sign(
            AuthenticatorRecord {
                version: AUTH_VERSION,
                account: i.account.clone(),
                credential_id: registration.credential_id().to_owned(),
                cose_public_key: registration.cose_public_key().to_vec(),
                rp_id: registration.relying_party().rp_id().to_owned(),
                origins: vec![registration.relying_party().origin().to_owned()],
                user_handle: registration.user_handle().to_owned(),
                backup_eligible: registration.backup_eligible(),
                initial_state: AuthenticatorState {
                    sign_count: registration.sign_count(),
                    backup_state: registration.backup_state(),
                },
                client_enrollment: PasskeyClientEnrollment::AccountClient,
                issued_under: head.clone(),
                not_before_ms: i.credential_not_before_ms,
                expires_at_ms: i.credential_expires_at_ms,
                nonce: envelope.nonce.clone(),
                enrollment_hash: registration.envelope_hash().map_err(|_| {
                    AuthorizationError::InvalidProof("invalid registration envelope hash")
                })?,
            },
            &verified.root,
        )?;
        verified.usage.merge(&policy_entry_usage_with_cache(
            &state,
            &JournalEntry::Authenticator(record.clone()),
            Some(&cached),
        )?);
        let hash = state.install_authenticator(record.clone(), now)?;
        let mut document = state
            .account(&i.account)
            .ok_or(AuthorizationError::Revoked)?
            .clone();
        document.previous = Some(head);
        document.sequence = document
            .sequence
            .checked_add(1)
            .ok_or(AuthorizationError::PolicyConflict)?;
        document.authenticators = vec![hash];
        document.authenticator_approval = None;
        document.device_enrollment = None;
        document.root_acceptance = None;
        document.recovery_approvals.clear();
        let proposal = Signed::sign(document, &verified.root)?;
        verified.usage.merge(&policy_entry_usage_with_cache(
            &state,
            &JournalEntry::Account(proposal.clone()),
            Some(&cached),
        )?);
        state.install_account(proposal.clone())?;
        registration
            .matches_authorized_record(&state.verify_authenticator(&record, now)?)
            .map_err(|_| {
                AuthorizationError::InvalidProof("first authenticator metadata changed")
            })?;
        let request = FirstRegistrationRequest {
            intent,
            record,
            proposal,
        };
        verified.usage.record_signed(&request.intent)?;
        verified.usage.record_signed(&request.record)?;
        verified.usage.record_signed(&request.proposal)?;
        require(
            canonical(&request)?.len() <= MAX_PROOF_BYTES,
            "first registration request too large",
        )?;
        finish_usage(&mut verified.usage)?;
        Ok(PreparedFirstAuthenticatorRegistration {
            request,
            source: verified.source,
            usage: verified.usage,
        })
    };
    prepare().map_err(|e| e.to_string())
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen)]
impl PreparedFirstAuthenticatorRegistration {
    #[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen(js_name = requestJson))]
    pub fn request_json(&self) -> crate::Result<String> {
        encode(&self.request)
    }
    #[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen(js_name = completionJournalJson))]
    pub fn completion_journal_json(&self) -> crate::Result<String> {
        encode(&[
            JournalEntry::Authenticator(self.request.record.clone()),
            JournalEntry::Account(self.request.proposal.clone()),
        ])
    }
    #[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen(js_name = proofUsageJson))]
    pub fn proof_usage_json(&self) -> crate::Result<String> {
        encode(&self.usage.counts())
    }
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen(js_name = finishFirstAuthenticatorRegistration))]
pub fn finish_first_authenticator_registration(
    prepared: &PreparedFirstAuthenticatorRegistration,
    committed_journal_json: &str,
    now_ms: f64,
) -> crate::Result<VerifiedFirstAuthenticatorRegistration> {
    let _scope = ScopedSignatureVerification::new();
    let finish = || -> Result<VerifiedFirstAuthenticatorRegistration> {
        require(
            committed_journal_json.len() <= MAX_PROOF_BYTES,
            "registration receipt too large",
        )?;
        let delta: Vec<JournalEntry> = serde_json::from_str(committed_journal_json)
            .map_err(|_| AuthorizationError::InvalidProof("invalid first registration receipt"))?;
        let [
            JournalEntry::Authenticator(record),
            JournalEntry::AccountCommit(change),
        ] = delta.as_slice()
        else {
            return Err(AuthorizationError::InvalidProof(
                "first registration receipt requires the exact committed delta",
            ));
        };
        require(
            record == &prepared.request.record && change.proposal == prepared.request.proposal,
            "first registration receipt changed the approved record or proposal",
        )?;
        let now = timestamp(now_ms)?;
        let accepted = change.commit.payload.accepted_at_ms;
        require(
            accepted <= now,
            "first registration receipt is from the future",
        )?;
        let mut snapshot = prepared.source.clone();
        let cached = CachedPolicyProof::from_verified_snapshot(&snapshot);
        let mut usage = ExecutionProofUsage::default();
        usage.record_signed(&prepared.request.intent)?;
        for entry in &delta {
            usage.merge(&policy_entry_usage_with_cache(
                snapshot.state(),
                entry,
                Some(&cached),
            )?);
            snapshot = snapshot.apply_delta(std::slice::from_ref(entry))?;
        }
        snapshot.state().verify_authenticator(record, accepted)?;
        let head = account_stamp(&change.proposal)?;
        let mut pins = snapshot.pins().to_vec();
        let pin = pins
            .iter_mut()
            .find(|pin| pin.domain == head.domain)
            .ok_or(AuthorizationError::Revoked)?;
        pin.minimum_head = Some(head);
        snapshot = snapshot.with_independent_pins(&pins)?;
        finish_usage(&mut usage)?;
        Ok(VerifiedFirstAuthenticatorRegistration { snapshot, usage })
    };
    finish().map_err(|e| e.to_string())
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen)]
impl VerifiedFirstAuthenticatorRegistration {
    #[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen(js_name = pinsJson))]
    pub fn pins_json(&self) -> crate::Result<String> {
        encode(&self.snapshot.pins())
    }
    #[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen(js_name = proofUsageJson))]
    pub fn proof_usage_json(&self) -> crate::Result<String> {
        encode(&self.usage.counts())
    }
}
