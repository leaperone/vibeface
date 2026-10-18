use super::passkey_store::{
    PasskeyContext, PasskeyDeviceCompletion, PasskeyManagementCompletion, PasskeyPreparation,
    PasskeyRegistrationCompletion,
};
use crate::{config::Config, crypto::LocalIdentity, store::Store};
use anyhow::{Context, Result, bail, ensure};
use serde_json::{Value, json};
use vibeface_secure::authorization::{
    AUTH_VERSION, AuthenticatorManagementIntent, DeviceId, MAX_PROOF_BYTES, ObjectHash, PublicKey,
    Signed,
    enrollment::{
        AuthenticatorEnrollmentIntent, DeviceEnrollmentIntent, verify_passkey_enrollment_request,
    },
    passkey_node::NodePasskeyPending,
};
use vibeface_secure::webauthn::{PasskeyPurpose, RelyingParty};

fn passkey_context(
    config: &Config,
    identity: &LocalIdentity,
    params: &Value,
) -> Result<PasskeyContext> {
    let primary = reqwest::Url::parse(&config.public_url)?
        .origin()
        .ascii_serialization();
    let origin = params["origin"].as_str().unwrap_or(&primary);
    ensure!(
        origin == primary
            || config
                .allowed_origins
                .iter()
                .any(|allowed| allowed == origin),
        "Passkey origin is not configured for this Node"
    );
    let url = reqwest::Url::parse(origin)?;
    ensure!(
        url.origin().ascii_serialization() == origin,
        "Passkey origin must be an exact configured origin"
    );
    Ok(PasskeyContext {
        target: DeviceId::from_key(&PublicKey::parse(&identity.public_key_hex()?)?),
        relying_party: RelyingParty::new(
            url.host_str().context("Passkey RP host is missing")?,
            origin,
        )
        .map_err(anyhow::Error::msg)?,
    })
}

fn preparation_response(
    prepared: PasskeyPreparation,
    signer: &vibeface_secure::Identity,
    now: u64,
) -> Result<Value> {
    ensure_preparation_carrier(&prepared)?;
    let mut response = serde_json::to_value(&prepared)?;
    if let PasskeyPreparation::Pending { pending, .. } = prepared {
        let expires_at_ms = pending.envelope().expires_at_ms;
        response["signed_pending"] = serde_json::to_value(Signed::sign(
            NodePasskeyPending {
                version: AUTH_VERSION,
                pending,
                issued_at_ms: now,
                expires_at_ms,
            },
            signer,
        )?)?;
    }
    Ok(response)
}

pub(super) fn ensure_preparation_carrier(prepared: &PasskeyPreparation) -> Result<()> {
    let mut carrier = serde_json::to_value(prepared)?;
    if let PasskeyPreparation::Pending { pending, .. } = prepared {
        let expires_at_ms = pending.envelope().expires_at_ms;
        // Ed25519 key and signature fields have fixed wire lengths; this value is only measured.
        carrier["signed_pending"] = json!({
            "payload":NodePasskeyPending {version:AUTH_VERSION,pending:pending.clone(),
                issued_at_ms:u64::MAX,expires_at_ms},
            "issuer":"0".repeat(64),"signature":"0".repeat(128),
        });
    }
    ensure!(
        serde_json::to_vec(&carrier)?.len() <= MAX_PROOF_BYTES,
        "Passkey preparation exceeds its bounded carrier"
    );
    Ok(())
}

impl Store {
    pub(crate) fn passkey_command(
        &mut self,
        config: &Config,
        identity: &LocalIdentity,
        principal: &str,
        method: &str,
        params: &Value,
        now: u64,
    ) -> Result<Value> {
        let _signature_scope =
            vibeface_secure::signature_verification::ScopedSignatureVerification::new();
        ensure!(
            serde_json::to_vec(params)?.len() <= MAX_PROOF_BYTES,
            "Passkey request exceeds its bounded carrier"
        );
        let context = passkey_context(config, identity, params)?;
        let signer = vibeface_secure::Identity::from_secret_hex(&identity.secret_key_hex)
            .map_err(anyhow::Error::msg)?;
        let response = match method {
            "passkey_prepare_registration" => {
                let intent: Signed<AuthenticatorEnrollmentIntent> =
                    serde_json::from_value(params["intent"].clone())?;
                ensure!(
                    intent.payload.requesting_device_public_key.as_str() == principal,
                    "Passkey requester differs from Noise device"
                );
                preparation_response(
                    self.prepare_passkey_registration(&intent, &context, now)?,
                    &signer,
                    now,
                )
            }
            "passkey_prepare_device" => {
                let intent: Signed<DeviceEnrollmentIntent> =
                    serde_json::from_value(params["intent"].clone())?;
                ensure!(
                    intent.payload.new_device_public_key.as_str() == principal,
                    "Passkey device differs from Noise device"
                );
                preparation_response(
                    self.prepare_passkey_device(&intent, &context, now)?,
                    &signer,
                    now,
                )
            }
            "passkey_prepare_management" => {
                let intent: Signed<AuthenticatorManagementIntent> =
                    serde_json::from_value(params["intent"].clone())?;
                ensure!(
                    intent.payload.device_public_key.as_str() == principal,
                    "Passkey manager differs from Noise device"
                );
                preparation_response(
                    self.prepare_passkey_management(&intent, &context, now)?,
                    &signer,
                    now,
                )
            }
            "passkey_verify_device" | "passkey_verify_management" => {
                let (account, hash, purpose) = if method == "passkey_verify_device" {
                    let intent: Signed<DeviceEnrollmentIntent> =
                        serde_json::from_value(params["intent"].clone())?;
                    ensure!(
                        intent.payload.new_device_public_key.as_str() == principal,
                        "Passkey device differs from Noise device"
                    );
                    let qualified =
                        verify_passkey_enrollment_request(self.policy.verified(), &intent, now)?;
                    (
                        qualified.account().clone(),
                        qualified.intent_hash().clone(),
                        PasskeyPurpose::EnrollDevice,
                    )
                } else {
                    let intent: Signed<AuthenticatorManagementIntent> =
                        serde_json::from_value(params["intent"].clone())?;
                    ensure!(
                        intent.payload.device_public_key.as_str() == principal,
                        "Passkey manager differs from Noise device"
                    );
                    let qualified = self
                        .policy
                        .verified()
                        .verify_authenticator_management_intent(&intent, now)?;
                    (
                        qualified.account().clone(),
                        qualified.intent_hash().clone(),
                        PasskeyPurpose::ManageAuthenticators,
                    )
                };
                let id: ObjectHash = serde_json::from_value(params["pending_id"].clone())?;
                let response = self.verify_passkey_response(
                    &account,
                    &id,
                    &serde_json::to_string(&params["response"])?,
                    &context,
                    now,
                )?;
                ensure!(
                    response.proof().pending.envelope().enrollment_hash == hash
                        && response.proof().pending.envelope().purpose == purpose,
                    "Passkey response belongs to another signed intent"
                );
                let statement = Signed::sign(response.node_state(now)?, &signer)?;
                Ok(json!({"proof":response.proof(),"signed_state":statement}))
            }
            "passkey_complete_registration" => {
                let completion: PasskeyRegistrationCompletion =
                    serde_json::from_value(params["completion"].clone())?;
                ensure!(
                    completion
                        .intent
                        .payload
                        .requesting_device_public_key
                        .as_str()
                        == principal,
                    "Passkey requester differs from Noise device"
                );
                Ok(serde_json::to_value(
                    self.complete_passkey_registration_at_authority(
                        &signer,
                        &completion,
                        &context,
                        now,
                    )?,
                )?)
            }
            "passkey_complete_device" => {
                let completion: PasskeyDeviceCompletion =
                    serde_json::from_value(params["completion"].clone())?;
                ensure!(
                    completion.intent.payload.new_device_public_key.as_str() == principal,
                    "Passkey device differs from Noise device"
                );
                Ok(serde_json::to_value(
                    self.complete_passkey_device_at_authority(&signer, &completion, &context, now)?,
                )?)
            }
            "passkey_complete_management" => {
                let completion: PasskeyManagementCompletion =
                    serde_json::from_value(params["completion"].clone())?;
                ensure!(
                    completion.intent.payload.device_public_key.as_str() == principal,
                    "Passkey manager differs from Noise device"
                );
                Ok(serde_json::to_value(
                    self.complete_passkey_management_at_authority(
                        &signer,
                        &completion,
                        &context,
                        now,
                    )?,
                )?)
            }
            _ => bail!("unknown Passkey method"),
        }?;
        ensure!(
            serde_json::to_vec(&response)?.len() <= MAX_PROOF_BYTES,
            "Passkey response exceeds its bounded carrier"
        );
        Ok(response)
    }
}
