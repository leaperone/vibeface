//! The first passkey is authorized inside the explicitly imported recovery material.

use super::*;
use crate::authorization::enrollment::{
    AuthenticatorEnrollmentIntent, verify_authenticator_enrollment,
};
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
    pins: Vec<PolicyPin>,
    bundle: ClientAuthorizationBundle,
}

#[derive(Clone, Debug)]
#[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen)]
pub struct VerifiedFirstAuthenticatorRegistration {
    pins: Vec<PolicyPin>,
    bundle: ClientAuthorizationBundle,
}

fn context(
    material_json: &str,
    pins_json: &str,
    bundle_json: &str,
) -> Result<(
    AccountId,
    Identity,
    VerifiedPolicySet,
    Vec<PolicyPin>,
    ClientAuthorizationBundle,
)> {
    let result = current_root_material_context(material_json, pins_json, bundle_json)?;
    let (account_id, _, state, _, _) = &result;
    let account = state
        .account(account_id)
        .ok_or(AuthorizationError::Revoked)?;
    let history = &state
        .accounts
        .get(&account.account)
        .ok_or(AuthorizationError::Revoked)?
        .history;
    require(
        history
            .iter()
            .filter(|doc| doc.payload.root_generation == account.root_generation)
            .all(|doc| doc.payload.authenticators.is_empty()),
        "recovery material is only used for the first authenticator of this root generation",
    )?;
    Ok(result)
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen(js_name = buildFirstAuthenticatorEnrollmentIntent))]
pub fn build_first_authenticator_enrollment_intent(
    material_json: &str,
    pins_json: &str,
    bundle_json: &str,
    requesting_client: &Identity,
    now_ms: f64,
    credential_expires_at_ms: f64,
) -> crate::Result<String> {
    let build = || -> Result<Signed<AuthenticatorEnrollmentIntent>> {
        let (expected_account, root, state, _, _) = context(material_json, pins_json, bundle_json)?;
        let certificate =
            state.find_device(&DeviceId::from_key(&PublicKey::of(requesting_client)))?;
        let Actor::Account(account) = &certificate.payload.subject else {
            return Err(AuthorizationError::InvalidProof(
                "first registration requires an account client",
            ));
        };
        require(
            account == &expected_account
                && state
                    .account(account)
                    .is_some_and(|doc| doc.current_root == PublicKey::of(&root)),
            "first registration client belongs to another account",
        )?;
        let document = state.account(account).ok_or(AuthorizationError::Revoked)?;
        let signed = Signed::sign(
            AuthenticatorEnrollmentIntent {
                account: account.clone(),
                requesting_device_public_key: PublicKey::of(requesting_client),
                target: document.authority.device.clone(),
                credential_not_before_ms: timestamp(now_ms)?,
                credential_expires_at_ms: timestamp(credential_expires_at_ms)?,
                expected_heads: vec![
                    state
                        .head(&PolicyDomain::Account(account.clone()))
                        .ok_or(AuthorizationError::Revoked)?
                        .clone(),
                ],
            },
            &root,
        )?;
        verify_authenticator_enrollment(&state, &signed, timestamp(now_ms)?)?;
        Ok(signed)
    };
    encode(&build().map_err(|e| e.to_string())?)
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen(js_name = prepareFirstAuthenticatorRegistration))]
pub fn prepare_first_authenticator_registration(
    material_json: &str,
    pins_json: &str,
    bundle_json: &str,
    signed_intent_json: &str,
    registration: &VerifiedPasskeyRegistration,
    now_ms: f64,
) -> crate::Result<PreparedFirstAuthenticatorRegistration> {
    let prepare = || -> Result<PreparedFirstAuthenticatorRegistration> {
        let now = timestamp(now_ms)?;
        let (expected_account, root, mut state, pins, bundle) =
            context(material_json, pins_json, bundle_json)?;
        let intent: Signed<AuthenticatorEnrollmentIntent> =
            decode_signed(signed_intent_json.as_bytes())?;
        let token = verify_authenticator_enrollment(&state, &intent, now)?;
        require(
            intent.issuer == PublicKey::of(&root),
            "first authenticator intent is not root approved",
        )?;
        let i = &intent.payload;
        require(
            i.account == expected_account
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
            &root,
        )?;
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
        let proposal = Signed::sign(document, &root)?;
        state.install_account(proposal.clone())?;
        registration
            .matches_authorized_record(&state.verify_authenticator(&record, now)?)
            .map_err(|_| {
                AuthorizationError::InvalidProof("first authenticator metadata changed")
            })?;
        Ok(PreparedFirstAuthenticatorRegistration {
            request: FirstRegistrationRequest {
                intent,
                record,
                proposal,
            },
            pins,
            bundle,
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
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen(js_name = finishFirstAuthenticatorRegistration))]
pub fn finish_first_authenticator_registration(
    prepared: &PreparedFirstAuthenticatorRegistration,
    committed_journal_json: &str,
    now_ms: f64,
) -> crate::Result<VerifiedFirstAuthenticatorRegistration> {
    let finish = || -> Result<VerifiedFirstAuthenticatorRegistration> {
        require(
            committed_journal_json.len() <= MAX_RECOVERY_JOURNAL_BYTES,
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
        let mut state = replay_policies(&prepared.pins, &prepared.bundle.journal)?;
        state.install_authenticator(record.clone(), accepted)?;
        let head = state.replay_account_committed(change.clone())?;
        state.verify_authenticator(record, accepted)?;
        let mut pins = prepared.pins.clone();
        let pin = pins
            .iter_mut()
            .find(|pin| pin.domain == head.domain)
            .ok_or(AuthorizationError::Revoked)?;
        pin.minimum_head = Some(head);
        let mut bundle = prepared.bundle.clone();
        bundle.journal.extend(delta);
        replay_policies(&pins, &bundle.journal)?;
        Ok(VerifiedFirstAuthenticatorRegistration { pins, bundle })
    };
    finish().map_err(|e| e.to_string())
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen)]
impl VerifiedFirstAuthenticatorRegistration {
    #[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen(js_name = pinsJson))]
    pub fn pins_json(&self) -> crate::Result<String> {
        encode(&self.pins)
    }
    #[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen(js_name = bundleJson))]
    pub fn bundle_json(&self) -> crate::Result<String> {
        encode(&self.bundle)
    }
}
