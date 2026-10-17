//! JSON boundaries for trusted Store pending records and shared passkey verification.

use anyhow::{Result, bail};
use vibeface_secure::{
    authorization::VerifiedAuthenticator,
    webauthn::{
        AssertionResponse, AuthenticatorState, PendingPasskey, RegisteredPasskey,
        RegistrationResponse, VerifiedPasskeyAssertion, VerifiedPasskeyRegistration,
        verify_passkey_assertion, verify_passkey_registration,
    },
};

pub fn verify_registration(
    pending: &PendingPasskey,
    response_json: &str,
    authorized_record: &VerifiedAuthenticator,
    now_ms: u64,
) -> Result<VerifiedPasskeyRegistration> {
    if response_json.len() > 65_536 {
        bail!("passkey registration response too large");
    }
    let response: RegistrationResponse = serde_json::from_str(response_json)?;
    let registration =
        verify_passkey_registration(pending, &response, now_ms).map_err(anyhow::Error::msg)?;
    registration
        .matches_authorized_record(authorized_record)
        .map_err(anyhow::Error::msg)?;
    Ok(registration)
}

pub fn verify_device_approval(
    pending: &PendingPasskey,
    response_json: &str,
    authorized_record: &VerifiedAuthenticator,
    state: AuthenticatorState,
    now_ms: u64,
) -> Result<VerifiedPasskeyAssertion> {
    if response_json.len() > 65_536 {
        bail!("passkey assertion response too large");
    }
    let response: AssertionResponse = serde_json::from_str(response_json)?;
    let credential =
        RegisteredPasskey::from_authorized(authorized_record, state).map_err(anyhow::Error::msg)?;
    verify_passkey_assertion(pending, &credential, &response, now_ms).map_err(anyhow::Error::msg)
}
