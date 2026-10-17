//! Shared WebAuthn ceremonies for the ES256, none-attestation profile.

use crate::authorization::{
    AUTH_VERSION, AccountId, AccountRight, DeviceId, Nonce, ObjectHash, PolicyStamp, PublicKey,
    VerifiedAuthenticator,
};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use coset::{CborOrdering, CborSerializable, CoseKey, Label, cbor::Value, iana};
use p256::ecdsa::{Signature, VerifyingKey, signature::Verifier};
use serde::{Deserialize, Deserializer, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;
use url::Url;

pub const PASSKEY_MAX_AGE_MS: u64 = 120_000;
type Result<T> = std::result::Result<T, String>;

fn require(condition: bool, reason: &str) -> Result<()> {
    if condition {
        Ok(())
    } else {
        Err(reason.to_owned())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PasskeyPurpose {
    RegisterAuthenticator,
    EnrollDevice,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct EnrollmentEnvelope {
    pub version: u16,
    pub purpose: PasskeyPurpose,
    pub account: AccountId,
    pub device_public_key: PublicKey,
    pub rights: Vec<AccountRight>,
    pub target: DeviceId,
    pub enrollment_hash: ObjectHash,
    pub expected_heads: Vec<PolicyStamp>,
    pub nonce: Nonce,
    pub not_before_ms: u64,
    pub expires_at_ms: u64,
}
impl EnrollmentEnvelope {
    pub fn hash(&self) -> Result<ObjectHash> {
        ObjectHash::body(self).map_err(|e| e.to_string())
    }
    fn validate(&self, now_ms: u64) -> Result<()> {
        require(
            self.version == AUTH_VERSION,
            "unsupported passkey envelope version",
        )?;
        require(
            self.not_before_ms <= now_ms && now_ms < self.expires_at_ms,
            "passkey request expired or not yet valid",
        )?;
        require(
            self.expires_at_ms - self.not_before_ms <= PASSKEY_MAX_AGE_MS,
            "passkey request validity too long",
        )?;
        require(
            self.rights.windows(2).all(|v| v[0] < v[1]),
            "passkey rights must be unique and ordered",
        )?;
        require(
            !self.expected_heads.is_empty()
                && self.expected_heads.len() <= 10
                && self
                    .expected_heads
                    .windows(2)
                    .all(|v| v[0].domain < v[1].domain),
            "invalid passkey policy heads",
        )
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RelyingParty {
    rp_id: String,
    origin: String,
}
impl RelyingParty {
    pub fn new(rp_id: &str, origin: &str) -> Result<Self> {
        let url = Url::parse(origin).map_err(|_| "invalid passkey origin")?;
        require(
            url.origin().ascii_serialization() == origin
                && !url.cannot_be_a_base()
                && url.username().is_empty()
                && url.password().is_none(),
            "passkey origin must be an exact serialized origin",
        )?;
        require(
            url.host_str() == Some(rp_id),
            "passkey RP ID must match the configured origin host",
        )?;
        require(
            url.scheme() == "https" || (url.scheme() == "http" && rp_id == "localhost"),
            "passkey requires HTTPS or localhost",
        )?;
        Ok(Self {
            rp_id: rp_id.to_owned(),
            origin: origin.to_owned(),
        })
    }
    pub fn rp_id(&self) -> &str {
        &self.rp_id
    }
    pub fn origin(&self) -> &str {
        &self.origin
    }
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AuthenticationCeremony {
    IdentifiedAccount,
    Discoverable,
}

/// Only restore this state from trusted pending storage, never from an RPC response body.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PendingPasskey {
    envelope: EnrollmentEnvelope,
    relying_party: RelyingParty,
    user_handle: String,
    ceremony: AuthenticationCeremony,
    credential_ids: Vec<String>,
}
impl PendingPasskey {
    pub fn prepare(
        mut envelope: EnrollmentEnvelope,
        relying_party: RelyingParty,
        user_handle: &[u8],
        ceremony: AuthenticationCeremony,
        credential_ids: Vec<String>,
        now_ms: u64,
    ) -> Result<Self> {
        envelope.nonce = Nonce::random().map_err(|e| e.to_string())?;
        envelope.validate(now_ms)?;
        require(
            !user_handle.is_empty() && user_handle.len() <= 64,
            "invalid passkey user handle",
        )?;
        RelyingParty::new(&relying_party.rp_id, &relying_party.origin)?;
        require(
            credential_ids.len() <= 64 && credential_ids.windows(2).all(|v| v[0] < v[1]),
            "invalid passkey credential list",
        )?;
        for id in &credential_ids {
            require(!decode(id, 1024)?.is_empty(), "empty passkey credential ID")?;
        }
        if envelope.purpose == PasskeyPurpose::EnrollDevice {
            require(
                (ceremony == AuthenticationCeremony::Discoverable) == credential_ids.is_empty(),
                "invalid allowed credentials for ceremony",
            )?;
        }
        Ok(Self {
            envelope,
            relying_party,
            user_handle: URL_SAFE_NO_PAD.encode(user_handle),
            ceremony,
            credential_ids,
        })
    }
    pub fn envelope(&self) -> &EnrollmentEnvelope {
        &self.envelope
    }
    pub fn envelope_hash(&self) -> Result<ObjectHash> {
        self.envelope.hash()
    }
    pub fn relying_party(&self) -> &RelyingParty {
        &self.relying_party
    }
    pub fn user_handle(&self) -> &str {
        &self.user_handle
    }
    fn challenge(&self) -> Result<String> {
        let bytes = crate::membership::canonical_json(self)
            .map_err(|_| "invalid passkey pending encoding")?;
        let mut hash = Sha256::new();
        hash.update(b"vibeface:webauthn:challenge:v2\0");
        hash.update(bytes);
        Ok(URL_SAFE_NO_PAD.encode(hash.finalize()))
    }
    pub fn creation_options(&self) -> Result<serde_json::Value> {
        require(
            self.envelope.purpose == PasskeyPurpose::RegisterAuthenticator,
            "wrong registration purpose",
        )?;
        Ok(serde_json::json!({"publicKey": {
            "rp":{"id":self.relying_party.rp_id,"name":"Vibeface"},
            "user":{"id":self.user_handle,"name":self.envelope.account.as_str(),"displayName":"Vibeface account"},
            "challenge":self.challenge()?,"pubKeyCredParams":[{"type":"public-key","alg":-7}],
            "timeout":PASSKEY_MAX_AGE_MS,"attestation":"none",
            "authenticatorSelection":{"residentKey":"required","requireResidentKey":true,"userVerification":"required"},
            "excludeCredentials":self.credential_ids.iter().map(|id| serde_json::json!({"type":"public-key","id":id})).collect::<Vec<_>>()
        }}))
    }
    pub fn authentication_options(&self) -> Result<serde_json::Value> {
        require(
            self.envelope.purpose == PasskeyPurpose::EnrollDevice,
            "wrong assertion purpose",
        )?;
        Ok(
            serde_json::json!({"publicKey":{"challenge":self.challenge()?,"rpId":self.relying_party.rp_id,"timeout":PASSKEY_MAX_AGE_MS,"userVerification":"required","allowCredentials":self.credential_ids.iter().map(|id|serde_json::json!({"type":"public-key","id":id})).collect::<Vec<_>>()}}),
        )
    }
    fn validate(&self, purpose: PasskeyPurpose, now_ms: u64) -> Result<()> {
        self.envelope.validate(now_ms)?;
        require(self.envelope.purpose == purpose, "wrong passkey purpose")?;
        RelyingParty::new(&self.relying_party.rp_id, &self.relying_party.origin)?;
        require(
            self.credential_ids.len() <= 64 && self.credential_ids.windows(2).all(|v| v[0] < v[1]),
            "invalid stored passkey credential list",
        )?;
        for id in &self.credential_ids {
            require(
                !decode(id, 1024)?.is_empty(),
                "invalid stored passkey credential ID",
            )?;
        }
        if self.envelope.purpose == PasskeyPurpose::EnrollDevice {
            require(
                (self.ceremony == AuthenticationCeremony::Discoverable)
                    == self.credential_ids.is_empty(),
                "invalid stored passkey ceremony",
            )?;
        }
        let handle = decode(&self.user_handle, 64)?;
        require(!handle.is_empty(), "invalid stored user handle")
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RegistrationResponse {
    pub id: String,
    pub raw_id: String,
    #[serde(rename = "type")]
    pub credential_type: String,
    pub response: AttestationResponse,
    #[serde(default)]
    pub client_extension_results: serde_json::Value,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AttestationResponse {
    #[serde(rename = "clientDataJSON")]
    pub client_data_json: String,
    pub attestation_object: String,
    #[serde(default)]
    pub transports: Vec<String>,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AssertionResponse {
    pub id: String,
    pub raw_id: String,
    #[serde(rename = "type")]
    pub credential_type: String,
    pub response: AuthenticatorAssertion,
    #[serde(default)]
    pub client_extension_results: serde_json::Value,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AuthenticatorAssertion {
    #[serde(rename = "clientDataJSON")]
    pub client_data_json: String,
    pub authenticator_data: String,
    pub signature: String,
    pub user_handle: Option<String>,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ClientData {
    #[serde(rename = "type")]
    ceremony_type: String,
    challenge: String,
    origin: String,
    #[serde(default)]
    cross_origin: bool,
    #[serde(default, rename = "topOrigin", deserialize_with = "reject_top_origin")]
    _top_origin: (),
}
fn reject_top_origin<'de, D: Deserializer<'de>>(_: D) -> std::result::Result<(), D::Error> {
    Err(serde::de::Error::custom("topOrigin is not supported"))
}
fn decode(value: &str, maximum: usize) -> Result<Vec<u8>> {
    require(
        value.len() <= maximum * 4 / 3 + 4,
        "passkey field too large",
    )?;
    let bytes = URL_SAFE_NO_PAD
        .decode(value)
        .map_err(|_| "invalid passkey base64url")?;
    require(
        bytes.len() <= maximum && URL_SAFE_NO_PAD.encode(&bytes) == value,
        "invalid canonical passkey base64url",
    )?;
    Ok(bytes)
}
fn client_data(pending: &PendingPasskey, encoded: &str, ceremony: &str) -> Result<Vec<u8>> {
    let bytes = decode(encoded, 4096)?;
    let text = std::str::from_utf8(&bytes).map_err(|_| "invalid passkey client JSON UTF-8")?;
    let data: ClientData = serde_json::from_str(text.strip_prefix('\u{feff}').unwrap_or(text))
        .map_err(|_| "invalid passkey client data")?;
    require(
        data.ceremony_type == ceremony && data.challenge == pending.challenge()?,
        "passkey ceremony or challenge mismatch",
    )?;
    require(
        data.origin == pending.relying_party.origin && !data.cross_origin,
        "passkey origin mismatch or cross-origin request",
    )?;
    Ok(bytes)
}
fn cbor_value(reader: &mut &[u8]) -> Result<Value> {
    coset::cbor::de::from_reader(reader).map_err(|_| "invalid passkey CBOR".to_owned())
}
fn public_key(bytes: &[u8]) -> Result<VerifyingKey> {
    let mut key = CoseKey::from_slice(bytes).map_err(|_| "invalid passkey COSE key")?;
    require(
        key.kty == coset::KeyType::Assigned(iana::KeyType::EC2)
            && key.alg == Some(coset::Algorithm::Assigned(iana::Algorithm::ES256)),
        "unsupported passkey algorithm",
    )?;
    require(
        key.key_id.is_empty()
            && key.key_ops.is_empty()
            && key.base_iv.is_empty()
            && key.params.len() == 3
            && key
                .params
                .iter()
                .any(|(k, v)| *k == Label::Int(-1) && *v == Value::Integer(1.into())),
        "unsupported passkey COSE fields",
    )?;
    key.canonicalize(CborOrdering::Lexicographic);
    require(
        key.clone()
            .to_vec()
            .map_err(|_| "invalid passkey COSE encoding")?
            == bytes,
        "passkey COSE key is not canonical",
    )?;
    let sec1 = key
        .to_sec1_octet_string()
        .map_err(|_| "invalid passkey coordinates")?;
    require(sec1.len() == 65, "passkey coordinates must be 32 bytes")?;
    VerifyingKey::from_sec1_bytes(&sec1).map_err(|_| "invalid passkey curve point".to_owned())
}
struct AuthenticatorData {
    counter: u32,
    backup_eligible: bool,
    backup_state: bool,
    credential_id: Option<Vec<u8>>,
    cose_public_key: Option<Vec<u8>>,
}
fn authenticator_data(
    pending: &PendingPasskey,
    bytes: &[u8],
    registration: bool,
) -> Result<AuthenticatorData> {
    require(
        bytes.len() >= 37 && bytes.len() <= 8192,
        "invalid authenticator data length",
    )?;
    require(
        bytes[..32] == Sha256::digest(pending.relying_party.rp_id.as_bytes())[..],
        "passkey RP ID hash mismatch",
    )?;
    let flags = bytes[32];
    require(
        flags & 1 != 0 && flags & 4 != 0,
        "passkey requires user presence and verification",
    )?;
    require(
        flags & 0x10 == 0 || flags & 8 != 0,
        "invalid passkey backup flags",
    )?;
    require(
        (flags & 0x40 != 0) == registration,
        "unexpected passkey attested data flag",
    )?;
    let mut tail = &bytes[37..];
    let (credential_id, cose_public_key) = if registration {
        require(tail.len() >= 18, "truncated passkey attested data")?;
        let length = u16::from_be_bytes([tail[16], tail[17]]) as usize;
        require(
            length > 0 && length <= 1024 && tail.len() >= 18 + length,
            "invalid passkey credential ID length",
        )?;
        let id = tail[18..18 + length].to_vec();
        tail = &tail[18 + length..];
        let before = tail;
        cbor_value(&mut tail)?;
        let key = before[..before.len() - tail.len()].to_vec();
        public_key(&key)?;
        (Some(id), Some(key))
    } else {
        (None, None)
    };
    if flags & 0x80 != 0 {
        let Value::Map(entries) = cbor_value(&mut tail)? else {
            return Err("passkey extensions must be a map".into());
        };
        let mut names = BTreeSet::new();
        for (name, _) in entries {
            let Value::Text(name) = name else {
                return Err("invalid passkey extension name".into());
            };
            require(names.insert(name), "duplicate passkey extension")?;
        }
    }
    require(
        tail.is_empty(),
        "unexpected passkey authenticator trailing bytes",
    )?;
    Ok(AuthenticatorData {
        counter: u32::from_be_bytes(bytes[33..37].try_into().unwrap()),
        backup_eligible: flags & 8 != 0,
        backup_state: flags & 0x10 != 0,
        credential_id,
        cose_public_key,
    })
}

#[derive(Clone, Debug)]
#[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen)]
pub struct VerifiedPasskeyRegistration {
    envelope: EnrollmentEnvelope,
    credential_id: String,
    cose_public_key: Vec<u8>,
    relying_party: RelyingParty,
    user_handle: String,
    sign_count: u32,
    backup_eligible: bool,
    backup_state: bool,
}
impl VerifiedPasskeyRegistration {
    pub fn matches_authorized_record(&self, record: &VerifiedAuthenticator) -> Result<()> {
        require(
            self.envelope.account == *record.account()
                && self.envelope_hash()? == record.record().payload.enrollment_hash
                && self.credential_id == record.credential_id()
                && self.cose_public_key == record.cose_public_key()
                && self.relying_party.rp_id == record.rp_id()
                && self.relying_party.origin == record.origin()
                && self.user_handle == record.user_handle()
                && self.backup_eligible == record.backup_eligible(),
            "authorized passkey record does not match registration",
        )
    }
    pub fn envelope(&self) -> &EnrollmentEnvelope {
        &self.envelope
    }
    pub fn envelope_hash(&self) -> Result<ObjectHash> {
        self.envelope.hash()
    }
    pub fn credential_id(&self) -> &str {
        &self.credential_id
    }
    pub fn cose_public_key(&self) -> &[u8] {
        &self.cose_public_key
    }
    pub fn relying_party(&self) -> &RelyingParty {
        &self.relying_party
    }
    pub fn user_handle(&self) -> &str {
        &self.user_handle
    }
    pub fn sign_count(&self) -> u32 {
        self.sign_count
    }
    pub fn backup_eligible(&self) -> bool {
        self.backup_eligible
    }
    pub fn backup_state(&self) -> bool {
        self.backup_state
    }
}

pub fn verify_passkey_registration(
    pending: &PendingPasskey,
    response: &RegistrationResponse,
    now_ms: u64,
) -> Result<VerifiedPasskeyRegistration> {
    pending.validate(PasskeyPurpose::RegisterAuthenticator, now_ms)?;
    require(
        response.credential_type == "public-key" && response.id == response.raw_id,
        "passkey credential type or ID mismatch",
    )?;
    let raw_id = decode(&response.raw_id, 1024)?;
    client_data(
        pending,
        &response.response.client_data_json,
        "webauthn.create",
    )?;
    let object_bytes = decode(&response.response.attestation_object, 16_384)?;
    let mut object_input = object_bytes.as_slice();
    let Value::Map(entries) = cbor_value(&mut object_input)? else {
        return Err("invalid passkey attestation object".into());
    };
    require(
        object_input.is_empty() && entries.len() == 3,
        "invalid passkey attestation object fields",
    )?;
    let (mut format, mut statement, mut data) = (None, None, None);
    for (name, value) in entries {
        match name {
            Value::Text(name) if name == "fmt" && format.is_none() => format = Some(value),
            Value::Text(name) if name == "attStmt" && statement.is_none() => {
                statement = Some(value)
            }
            Value::Text(name) if name == "authData" && data.is_none() => data = Some(value),
            _ => return Err("duplicate or unknown attestation field".into()),
        }
    }
    require(
        format == Some(Value::Text("none".into())) && statement == Some(Value::Map(vec![])),
        "only none passkey attestation is supported",
    )?;
    let Some(Value::Bytes(data)) = data else {
        return Err("missing passkey authenticator data".into());
    };
    let verified = authenticator_data(pending, &data, true)?;
    require(
        verified.credential_id.as_ref() == Some(&raw_id),
        "attested passkey credential ID mismatch",
    )?;
    require(
        !pending.credential_ids.contains(&response.raw_id),
        "passkey credential is excluded",
    )?;
    Ok(VerifiedPasskeyRegistration {
        envelope: pending.envelope.clone(),
        credential_id: response.raw_id.clone(),
        cose_public_key: verified.cose_public_key.unwrap(),
        relying_party: pending.relying_party.clone(),
        user_handle: pending.user_handle.clone(),
        sign_count: verified.counter,
        backup_eligible: verified.backup_eligible,
        backup_state: verified.backup_state,
    })
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct AuthenticatorState {
    pub sign_count: u32,
    pub backup_state: bool,
}

#[derive(Clone, Debug)]
pub struct RegisteredPasskey {
    account: AccountId,
    record_hash: ObjectHash,
    credential_id: String,
    cose_public_key: Vec<u8>,
    relying_party: RelyingParty,
    user_handle: String,
    backup_eligible: bool,
    not_before_ms: u64,
    expires_at_ms: u64,
    state: AuthenticatorState,
}
impl RegisteredPasskey {
    pub fn from_authorized(
        record: &VerifiedAuthenticator,
        state: AuthenticatorState,
    ) -> Result<Self> {
        public_key(record.cose_public_key())?;
        let relying_party = RelyingParty::new(record.rp_id(), record.origin())?;
        require(
            !decode(record.credential_id(), 1024)?.is_empty()
                && decode(record.user_handle(), 32)?.len() == 32,
            "invalid authorized passkey metadata",
        )?;
        require(
            !state.backup_state || record.backup_eligible(),
            "invalid stored passkey backup state",
        )?;
        Ok(Self {
            account: record.account().clone(),
            record_hash: record.record_hash().clone(),
            credential_id: record.credential_id().to_owned(),
            cose_public_key: record.cose_public_key().to_vec(),
            relying_party,
            user_handle: record.user_handle().to_owned(),
            backup_eligible: record.backup_eligible(),
            not_before_ms: record.record().payload.not_before_ms,
            expires_at_ms: record.expires_at_ms(),
            state,
        })
    }
    pub fn record_hash(&self) -> &ObjectHash {
        &self.record_hash
    }
    pub fn credential_id(&self) -> &str {
        &self.credential_id
    }
    pub fn state(&self) -> AuthenticatorState {
        self.state
    }
}

#[derive(Clone, Debug)]
#[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen)]
pub struct VerifiedPasskeyAssertion {
    envelope: EnrollmentEnvelope,
    record_hash: ObjectHash,
    credential_id: String,
    user_handle: String,
    previous_state: AuthenticatorState,
    sign_count: u32,
    backup_eligible: bool,
    backup_state: bool,
}
impl VerifiedPasskeyAssertion {
    pub fn envelope(&self) -> &EnrollmentEnvelope {
        &self.envelope
    }
    pub fn envelope_hash(&self) -> Result<ObjectHash> {
        self.envelope.hash()
    }
    pub fn account(&self) -> &AccountId {
        &self.envelope.account
    }
    pub fn nonce(&self) -> &Nonce {
        &self.envelope.nonce
    }
    pub fn expires_at_ms(&self) -> u64 {
        self.envelope.expires_at_ms
    }
    pub fn record_hash(&self) -> &ObjectHash {
        &self.record_hash
    }
    pub fn credential_id(&self) -> &str {
        &self.credential_id
    }
    pub fn user_handle(&self) -> &str {
        &self.user_handle
    }
    pub fn previous_state(&self) -> AuthenticatorState {
        self.previous_state
    }
    pub fn sign_count(&self) -> u32 {
        self.sign_count
    }
    pub fn backup_eligible(&self) -> bool {
        self.backup_eligible
    }
    pub fn backup_state(&self) -> bool {
        self.backup_state
    }
}

pub fn verify_passkey_assertion(
    pending: &PendingPasskey,
    credential: &RegisteredPasskey,
    response: &AssertionResponse,
    now_ms: u64,
) -> Result<VerifiedPasskeyAssertion> {
    pending.validate(PasskeyPurpose::EnrollDevice, now_ms)?;
    require(
        credential.account == pending.envelope.account
            && credential.relying_party == pending.relying_party
            && credential.user_handle == pending.user_handle,
        "passkey registered account or RP mismatch",
    )?;
    require(
        credential.not_before_ms <= now_ms && now_ms < credential.expires_at_ms,
        "passkey credential expired or not yet valid",
    )?;
    require(
        response.credential_type == "public-key"
            && response.id == response.raw_id
            && response.raw_id == credential.credential_id,
        "passkey credential type or ID mismatch",
    )?;
    if pending.ceremony == AuthenticationCeremony::IdentifiedAccount {
        require(
            pending.credential_ids.contains(&response.raw_id),
            "passkey credential was not allowed",
        )?;
    } else {
        require(
            response.response.user_handle.is_some(),
            "discoverable passkey requires user handle",
        )?;
    }
    if let Some(handle) = &response.response.user_handle {
        require(
            handle == &credential.user_handle
                && decode(handle, 64)? == decode(&credential.user_handle, 64)?,
            "passkey user handle mismatch",
        )?;
    }
    let client_bytes = client_data(pending, &response.response.client_data_json, "webauthn.get")?;
    let auth_bytes = decode(&response.response.authenticator_data, 8192)?;
    let data = authenticator_data(pending, &auth_bytes, false)?;
    require(
        data.backup_eligible == credential.backup_eligible,
        "passkey backup eligibility changed",
    )?;
    require(
        (data.counter == 0 && credential.state.sign_count == 0)
            || data.counter > credential.state.sign_count,
        "passkey signature counter did not advance",
    )?;
    let signature = Signature::from_der(&decode(&response.response.signature, 80)?)
        .map_err(|_| "invalid passkey DER signature")?;
    let mut signed = auth_bytes;
    signed.extend_from_slice(&Sha256::digest(&client_bytes));
    public_key(&credential.cose_public_key)?
        .verify(&signed, &signature)
        .map_err(|_| "invalid passkey signature")?;
    Ok(VerifiedPasskeyAssertion {
        envelope: pending.envelope.clone(),
        record_hash: credential.record_hash.clone(),
        credential_id: credential.credential_id.clone(),
        user_handle: credential.user_handle.clone(),
        previous_state: credential.state,
        sign_count: data.counter,
        backup_eligible: data.backup_eligible,
        backup_state: data.backup_state,
    })
}

fn timestamp(value: f64) -> Result<u64> {
    require(
        value.is_finite()
            && value >= 0.0
            && value.fract() == 0.0
            && value <= 9_007_199_254_740_991.0,
        "invalid passkey verification timestamp",
    )?;
    Ok(value as u64)
}
fn decode_json<T: for<'de> Deserialize<'de>>(json: &str, limit: usize) -> Result<T> {
    require(json.len() <= limit, "passkey JSON input too large")?;
    serde_json::from_str(json).map_err(|_| "invalid passkey JSON input".to_owned())
}
fn expected_pending(
    pending: &PendingPasskey,
    expected_hash: &str,
    rp_id: &str,
    origin: &str,
) -> Result<()> {
    require(
        pending.envelope_hash()?.as_str() == expected_hash
            && pending.relying_party == RelyingParty::new(rp_id, origin)?,
        "passkey pending request does not match expected request or client origin",
    )
}
#[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen)]
impl VerifiedPasskeyRegistration {
    #[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen(js_name = credentialJson))]
    pub fn credential_json(&self) -> Result<String> {
        serde_json::to_string(&serde_json::json!({"account":self.envelope.account,"enrollment_hash":self.envelope_hash()?,"credential_id":self.credential_id,"cose_public_key":self.cose_public_key,"rp_id":self.relying_party.rp_id,"origins":[self.relying_party.origin],"user_handle":self.user_handle,"backup_eligible":self.backup_eligible,"sign_count":self.sign_count,"backup_state":self.backup_state})).map_err(|_| "passkey credential encoding failed".to_owned())
    }
}
#[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen)]
impl VerifiedPasskeyAssertion {
    #[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen(js_name = proofJson))]
    pub fn proof_json(&self) -> Result<String> {
        serde_json::to_string(&serde_json::json!({"account":self.envelope.account,"envelope_hash":self.envelope_hash()?,"record_hash":self.record_hash,"credential_id":self.credential_id,"user_handle":self.user_handle,"sign_count":self.sign_count,"backup_eligible":self.backup_eligible,"backup_state":self.backup_state})).map_err(|_| "passkey proof encoding failed".to_owned())
    }
}
#[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen(js_name = verifyPasskeyRegistration))]
pub fn verify_registration_json(
    pending_json: &str,
    response_json: &str,
    expected_envelope_hash: &str,
    expected_rp_id: &str,
    expected_origin: &str,
    now_ms: f64,
) -> Result<VerifiedPasskeyRegistration> {
    let pending: PendingPasskey = decode_json(pending_json, 65_536)?;
    expected_pending(
        &pending,
        expected_envelope_hash,
        expected_rp_id,
        expected_origin,
    )?;
    verify_passkey_registration(
        &pending,
        &decode_json(response_json, 65_536)?,
        timestamp(now_ms)?,
    )
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PasskeyAssertionProof {
    pub pending: PendingPasskey,
    pub record: crate::authorization::Signed<crate::authorization::AuthenticatorRecord>,
    pub state: AuthenticatorState,
    pub response: AssertionResponse,
}
/// Browser pins and the ledger come from the saved account. The Node re-verifies its own current state.
#[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen(js_name = verifyPasskeyAssertion))]
pub fn verify_assertion_json(
    proof_json: &str,
    trusted_state_json: &str,
    pinned_roots_json: &str,
    journal_json: &str,
    expected_envelope_hash: &str,
    expected_rp_id: &str,
    expected_origin: &str,
    now_ms: f64,
) -> Result<VerifiedPasskeyAssertion> {
    let proof: PasskeyAssertionProof = decode_json(proof_json, 65_536)?;
    let trusted_state: AuthenticatorState = decode_json(trusted_state_json, 1024)?;
    require(
        proof.state == trusted_state,
        "passkey proof ledger does not match independent trusted state",
    )?;
    expected_pending(
        &proof.pending,
        expected_envelope_hash,
        expected_rp_id,
        expected_origin,
    )?;
    let pins: Vec<crate::authorization::PolicyPin> = decode_json(pinned_roots_json, 65_536)?;
    let journal: Vec<crate::authorization::JournalEntry> = decode_json(journal_json, 1_048_576)?;
    require(journal.len() <= 128, "passkey policy journal too large")?;
    let now_ms = timestamp(now_ms)?;
    let policies =
        crate::authorization::replay_policies(&pins, &journal).map_err(|e| e.to_string())?;
    let authorized = policies
        .verify_authenticator(&proof.record, now_ms)
        .map_err(|e| e.to_string())?;
    let credential = RegisteredPasskey::from_authorized(&authorized, trusted_state)?;
    verify_passkey_assertion(&proof.pending, &credential, &proof.response, now_ms)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::{Identity, authorization::PolicyDomain};
    use coset::CoseKeyBuilder;
    use p256::ecdsa::{SigningKey, signature::Signer};
    use serde_json::json;

    const NOW: u64 = 1000;
    fn bytes(value: &Value) -> Vec<u8> {
        let mut result = vec![];
        coset::cbor::ser::into_writer(value, &mut result).unwrap();
        result
    }
    fn pending(purpose: PasskeyPurpose, ceremony: AuthenticationCeremony) -> PendingPasskey {
        let identity = Identity::from_secret_hex(&"01".repeat(32)).unwrap();
        let key = PublicKey::of(&identity);
        let account = AccountId::from_root(&key);
        let envelope = EnrollmentEnvelope {
            version: AUTH_VERSION,
            purpose: purpose.clone(),
            account: account.clone(),
            device_public_key: key.clone(),
            rights: vec![],
            target: DeviceId::from_key(&key),
            enrollment_hash: ObjectHash::body(&"complete enrollment body").unwrap(),
            expected_heads: vec![PolicyStamp {
                domain: PolicyDomain::Account(account),
                authority_epoch: 0,
                sequence: 0,
                head: ObjectHash::body(&"head").unwrap(),
            }],
            nonce: Nonce::from_bytes([0; 32]),
            not_before_ms: NOW,
            expires_at_ms: NOW + PASSKEY_MAX_AGE_MS,
        };
        PendingPasskey::prepare(
            envelope,
            RelyingParty::new("client.example", "https://client.example").unwrap(),
            &[7; 32],
            ceremony,
            if purpose == PasskeyPurpose::EnrollDevice
                && ceremony == AuthenticationCeremony::IdentifiedAccount
            {
                vec![URL_SAFE_NO_PAD.encode([9; 32])]
            } else {
                vec![]
            },
            NOW,
        )
        .unwrap()
    }
    fn key() -> SigningKey {
        SigningKey::from_bytes((&[42; 32]).into()).unwrap()
    }
    pub(crate) fn cose() -> Vec<u8> {
        let point = key().verifying_key().to_encoded_point(false);
        CoseKeyBuilder::new_ec2_pub_key(
            iana::EllipticCurve::P_256,
            point.x().unwrap().to_vec(),
            point.y().unwrap().to_vec(),
        )
        .algorithm(iana::Algorithm::ES256)
        .build()
        .to_vec()
        .unwrap()
    }
    pub(crate) fn client(p: &PendingPasskey, ceremony: &str) -> Vec<u8> {
        serde_json::to_vec(&json!({"type":ceremony,"challenge":p.challenge().unwrap(),"origin":p.relying_party.origin,"crossOrigin":false})).unwrap()
    }
    pub(crate) fn auth(p: &PendingPasskey, flags: u8, counter: u32) -> Vec<u8> {
        let mut result = Sha256::digest(p.relying_party.rp_id.as_bytes()).to_vec();
        result.push(flags);
        result.extend_from_slice(&counter.to_be_bytes());
        result
    }
    fn registration(p: &PendingPasskey) -> RegistrationResponse {
        let mut data = auth(p, 0x4d, 0);
        data.extend([0; 16]);
        data.extend(32u16.to_be_bytes());
        data.extend([9; 32]);
        data.extend(cose());
        let object = Value::Map(vec![
            (Value::Text("fmt".into()), Value::Text("none".into())),
            (Value::Text("authData".into()), Value::Bytes(data)),
            (Value::Text("attStmt".into()), Value::Map(vec![])),
        ]);
        RegistrationResponse {
            id: URL_SAFE_NO_PAD.encode([9; 32]),
            raw_id: URL_SAFE_NO_PAD.encode([9; 32]),
            credential_type: "public-key".into(),
            response: AttestationResponse {
                client_data_json: URL_SAFE_NO_PAD.encode(client(p, "webauthn.create")),
                attestation_object: URL_SAFE_NO_PAD.encode(bytes(&object)),
                transports: vec![],
            },
            client_extension_results: json!({}),
        }
    }
    pub(crate) fn assertion(
        _p: &PendingPasskey,
        data: Vec<u8>,
        client: Vec<u8>,
        handle: Option<String>,
    ) -> AssertionResponse {
        let mut signed = data.clone();
        signed.extend_from_slice(&Sha256::digest(&client));
        let signature: Signature = key().sign(&signed);
        AssertionResponse {
            id: URL_SAFE_NO_PAD.encode([9; 32]),
            raw_id: URL_SAFE_NO_PAD.encode([9; 32]),
            credential_type: "public-key".into(),
            response: AuthenticatorAssertion {
                client_data_json: URL_SAFE_NO_PAD.encode(client),
                authenticator_data: URL_SAFE_NO_PAD.encode(data),
                signature: URL_SAFE_NO_PAD.encode(signature.to_der()),
                user_handle: handle,
            },
            client_extension_results: json!({}),
        }
    }
    fn registered(p: &PendingPasskey) -> RegisteredPasskey {
        RegisteredPasskey {
            account: p.envelope.account.clone(),
            record_hash: ObjectHash::body(&"signed root-authorized credential").unwrap(),
            credential_id: URL_SAFE_NO_PAD.encode([9; 32]),
            cose_public_key: cose(),
            relying_party: p.relying_party.clone(),
            user_handle: p.user_handle.clone(),
            backup_eligible: true,
            not_before_ms: NOW,
            expires_at_ms: NOW + 1000000,
            state: AuthenticatorState {
                sign_count: 0,
                backup_state: false,
            },
        }
    }
    fn edit_registration(response: &mut RegistrationResponse, edit: impl FnOnce(&mut Vec<u8>)) {
        let data = decode(&response.response.attestation_object, 16384).unwrap();
        let mut input = data.as_slice();
        let Value::Map(mut map) = cbor_value(&mut input).unwrap() else {
            panic!()
        };
        let (_, Value::Bytes(auth)) = map
            .iter_mut()
            .find(|(k, _)| *k == Value::Text("authData".into()))
            .unwrap()
        else {
            panic!()
        };
        edit(auth);
        response.response.attestation_object = URL_SAFE_NO_PAD.encode(bytes(&Value::Map(map)));
    }
    #[cfg_attr(not(target_arch = "wasm32"), test)]
    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    fn registration_checks_the_complete_none_attestation_ceremony() {
        let p = pending(
            PasskeyPurpose::RegisterAuthenticator,
            AuthenticationCeremony::IdentifiedAccount,
        );
        let original = registration(&p);
        let valid = verify_passkey_registration(&p, &original, NOW).unwrap();
        let pending_json = serde_json::to_string(&p).unwrap();
        let response_json = serde_json::to_string(&original).unwrap();
        let expected = p.envelope_hash().unwrap();
        assert!(
            verify_registration_json(
                &pending_json,
                &response_json,
                expected.as_str(),
                "client.example",
                "https://client.example",
                NOW as f64
            )
            .unwrap()
            .credential_json()
            .unwrap()
            .contains("credential_id")
        );
        assert!(
            verify_registration_json(
                &pending_json,
                &response_json,
                expected.as_str(),
                "other.example",
                "https://other.example",
                NOW as f64
            )
            .is_err()
        );
        assert!(
            verify_registration_json(
                &pending_json,
                &response_json,
                expected.as_str(),
                "client.example",
                "https://client.example",
                f64::NAN
            )
            .is_err()
        );
        assert_eq!(valid.credential_id(), URL_SAFE_NO_PAD.encode([9; 32]));
        assert_eq!(valid.cose_public_key(), cose());
        assert!(valid.backup_eligible());
        assert!(!valid.backup_state());
        for (name, edit) in [
            ("rp hash", 0),
            ("UP", 1),
            ("UV", 2),
            ("AT", 3),
            ("invalid BS", 4),
            ("credential length", 5),
            ("trailing bytes", 6),
            ("wrong credential", 7),
            ("private COSE field", 8),
            ("noncanonical COSE", 9),
        ] {
            let mut r = original.clone();
            edit_registration(&mut r, |d| match edit {
                0 => d[0] ^= 1,
                1 => d[32] &= !1,
                2 => d[32] &= !4,
                3 => d[32] &= !0x40,
                4 => d[32] = (d[32] | 0x10) & !8,
                5 => d[53] = 255,
                6 => d.push(0),
                7 => d[55] ^= 1,
                8 => {
                    let mut k = CoseKey::from_slice(&d[87..]).unwrap();
                    k.params.push((Label::Int(-4), Value::Bytes(vec![1; 32])));
                    d.truncate(87);
                    d.extend(k.to_vec().unwrap());
                }
                _ => {
                    let mut k = CoseKey::from_slice(&d[87..]).unwrap();
                    k.params.reverse();
                    d.truncate(87);
                    d.extend(k.to_vec().unwrap());
                }
            });
            assert!(
                verify_passkey_registration(&p, &r, NOW).is_err(),
                "accepted {name}"
            );
        }
        let mut r = original.clone();
        r.raw_id = URL_SAFE_NO_PAD.encode([8; 32]);
        r.id = r.raw_id.clone();
        assert!(verify_passkey_registration(&p, &r, NOW).is_err());
        let mut r = original.clone();
        let data = decode(&r.response.attestation_object, 16384).unwrap();
        let mut input = data.as_slice();
        let Value::Map(mut map) = cbor_value(&mut input).unwrap() else {
            panic!()
        };
        map[0].1 = Value::Text("packed".into());
        r.response.attestation_object = URL_SAFE_NO_PAD.encode(bytes(&Value::Map(map)));
        assert!(verify_passkey_registration(&p, &r, NOW).is_err());
        assert!(verify_passkey_registration(&p, &original, p.envelope.expires_at_ms).is_err());
    }
    #[cfg_attr(not(target_arch = "wasm32"), test)]
    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    fn assertions_check_origin_handle_backup_counter_and_full_request_binding() {
        let p = pending(
            PasskeyPurpose::EnrollDevice,
            AuthenticationCeremony::IdentifiedAccount,
        );
        let credential = registered(&p);
        let data = auth(&p, 0x0d, 0);
        let c = client(&p, "webauthn.get");
        let original = assertion(&p, data.clone(), c.clone(), None);
        let result = verify_passkey_assertion(&p, &credential, &original, NOW).unwrap();
        assert_eq!(result.envelope_hash().unwrap(), p.envelope_hash().unwrap());
        assert_eq!(result.previous_state().sign_count, 0);
        assert_eq!(result.sign_count(), 0);
        let cjson: serde_json::Value = serde_json::from_slice(&c).unwrap();
        for (name, value) in [
            ("origin", json!("https://attacker.example")),
            ("challenge", json!("bad challenge")),
            ("type", json!("webauthn.create")),
            ("crossOrigin", json!(true)),
            ("crossOrigin", json!(null)),
            ("topOrigin", json!(null)),
        ] {
            let mut changed = cjson.clone();
            changed[name] = value;
            let r = assertion(
                &p,
                data.clone(),
                serde_json::to_vec(&changed).unwrap(),
                None,
            );
            assert!(
                verify_passkey_assertion(&p, &credential, &r, NOW).is_err(),
                "accepted client {name}"
            );
        }
        let mut bom = vec![0xef, 0xbb, 0xbf];
        bom.extend(&c);
        assert!(
            verify_passkey_assertion(
                &p,
                &credential,
                &assertion(&p, data.clone(), bom, None),
                NOW
            )
            .is_ok()
        );
        for flags in [0x0c, 0x09, 0x05, 0x15, 0x4d] {
            assert!(
                verify_passkey_assertion(
                    &p,
                    &credential,
                    &assertion(&p, auth(&p, flags, 0), c.clone(), None),
                    NOW
                )
                .is_err(),
                "accepted flags {flags}"
            );
        }
        let mut counted = credential.clone();
        counted.state.sign_count = 3;
        assert!(
            verify_passkey_assertion(
                &p,
                &counted,
                &assertion(&p, auth(&p, 0x0d, 3), c.clone(), None),
                NOW
            )
            .is_err()
        );
        assert_eq!(
            verify_passkey_assertion(
                &p,
                &counted,
                &assertion(&p, auth(&p, 0x1d, 4), c.clone(), None),
                NOW
            )
            .unwrap()
            .sign_count(),
            4
        );
        let mut r = original.clone();
        r.response.user_handle = Some(URL_SAFE_NO_PAD.encode([6; 32]));
        assert!(verify_passkey_assertion(&p, &credential, &r, NOW).is_err());
        let mut changed = p.clone();
        changed.envelope.enrollment_hash = ObjectHash::body(&"changed rights body").unwrap();
        assert!(verify_passkey_assertion(&changed, &credential, &original, NOW).is_err());
        changed = p.clone();
        changed.envelope.target = DeviceId::from_key(&PublicKey::of(
            &Identity::from_secret_hex(&"02".repeat(32)).unwrap(),
        ));
        assert!(verify_passkey_assertion(&changed, &credential, &original, NOW).is_err());
        let mut r = original.clone();
        r.response.signature = URL_SAFE_NO_PAD.encode([1; 64]);
        assert!(verify_passkey_assertion(&p, &credential, &r, NOW).is_err());
        let discoverable = pending(
            PasskeyPurpose::EnrollDevice,
            AuthenticationCeremony::Discoverable,
        );
        let credential = registered(&discoverable);
        let r = assertion(
            &discoverable,
            auth(&discoverable, 0x0d, 0),
            client(&discoverable, "webauthn.get"),
            None,
        );
        assert!(verify_passkey_assertion(&discoverable, &credential, &r, NOW).is_err());
        let mut r = r;
        r.response.user_handle = Some(discoverable.user_handle.clone());
        assert!(verify_passkey_assertion(&discoverable, &credential, &r, NOW).is_ok());
    }
    #[cfg_attr(not(target_arch = "wasm32"), test)]
    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    fn extensions_and_json_are_consumed_without_ambiguous_fields() {
        let p = pending(
            PasskeyPurpose::EnrollDevice,
            AuthenticationCeremony::IdentifiedAccount,
        );
        let credential = registered(&p);
        let c = client(&p, "webauthn.get");
        let mut d = auth(&p, 0x8d, 0);
        d.extend(bytes(&Value::Map(vec![(
            Value::Text("ignored".into()),
            Value::Bool(true),
        )])));
        assert!(
            verify_passkey_assertion(
                &p,
                &credential,
                &assertion(&p, d.clone(), c.clone(), None),
                NOW
            )
            .is_ok()
        );
        d.push(0);
        assert!(
            verify_passkey_assertion(&p, &credential, &assertion(&p, d, c.clone(), None), NOW)
                .is_err()
        );
        let mut d = auth(&p, 0x8d, 0);
        d.extend(bytes(&Value::Map(vec![
            (Value::Text("dup".into()), Value::Bool(true)),
            (Value::Text("dup".into()), Value::Bool(false)),
        ])));
        assert!(
            verify_passkey_assertion(&p, &credential, &assertion(&p, d, c.clone(), None), NOW)
                .is_err()
        );
        let duplicate = format!(
            "{{\"type\":\"webauthn.get\",\"type\":\"webauthn.get\",\"challenge\":\"{}\",\"origin\":\"{}\"}}",
            p.challenge().unwrap(),
            p.relying_party.origin
        );
        assert!(
            verify_passkey_assertion(
                &p,
                &credential,
                &assertion(&p, auth(&p, 0x0d, 0), duplicate.into_bytes(), None),
                NOW
            )
            .is_err()
        );
        assert!(RelyingParty::new("example.com", "https://client.example.com").is_err());
        assert!(RelyingParty::new("client.example", "http://client.example").is_err());
        assert!(RelyingParty::new("localhost", "http://localhost:3010").is_ok());
        let serialized = serde_json::to_value(&p).unwrap();
        let restored: PendingPasskey = serde_json::from_value(serialized).unwrap();
        assert_eq!(p.challenge().unwrap(), restored.challenge().unwrap());
        let next = pending(
            PasskeyPurpose::EnrollDevice,
            AuthenticationCeremony::IdentifiedAccount,
        );
        assert_ne!(p.envelope.nonce, next.envelope.nonce);
    }
    #[cfg_attr(not(target_arch = "wasm32"), test)]
    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    fn only_a_root_authorized_active_record_can_construct_a_registered_passkey() {
        use crate::authorization::{
            AccountDocument, AuthenticatorRecord, AuthorityBinding, JournalEntry, PolicyPin,
            RecoveryPolicy, Signed, VerifiedPolicySet,
        };
        let registration_pending = pending(
            PasskeyPurpose::RegisterAuthenticator,
            AuthenticationCeremony::IdentifiedAccount,
        );
        let proof = verify_passkey_registration(
            &registration_pending,
            &registration(&registration_pending),
            NOW,
        )
        .unwrap();
        let root = Identity::from_secret_hex(&"01".repeat(32)).unwrap();
        let node = Identity::from_secret_hex(&"02".repeat(32)).unwrap();
        let recovery = Identity::from_secret_hex(&"03".repeat(32)).unwrap();
        let account = registration_pending.envelope.account.clone();
        let mut document = AccountDocument {
            version: AUTH_VERSION,
            account: account.clone(),
            genesis_root: PublicKey::of(&root),
            current_root: PublicKey::of(&root),
            root_generation: 0,
            previous: None,
            authority_epoch: 0,
            sequence: 0,
            authority: AuthorityBinding::from_key(PublicKey::of(&node)),
            recovery: RecoveryPolicy {
                generation: 0,
                keys: vec![PublicKey::of(&recovery)],
                threshold: 1,
            },
            enrollment: vec![],
            devices: vec![],
            revoked_devices: vec![],
            authenticators: vec![],
            root_acceptance: None,
            recovery_approvals: vec![],
        };
        let genesis = Signed::sign(document.clone(), &root).unwrap();
        let mut state = VerifiedPolicySet::new();
        let initial = state
            .trust_account_genesis(&PublicKey::of(&root), genesis.clone())
            .unwrap();
        let record = AuthenticatorRecord {
            version: AUTH_VERSION,
            account: account.clone(),
            credential_id: proof.credential_id().to_owned(),
            cose_public_key: proof.cose_public_key().to_vec(),
            rp_id: proof.relying_party().rp_id().to_owned(),
            origins: vec![proof.relying_party().origin().to_owned()],
            user_handle: proof.user_handle().to_owned(),
            backup_eligible: proof.backup_eligible(),
            issued_under: initial.clone(),
            not_before_ms: NOW,
            expires_at_ms: NOW + 1000000,
            nonce: registration_pending.envelope.nonce.clone(),
            enrollment_hash: proof.envelope_hash().unwrap(),
        };
        let signed = Signed::sign(record.clone(), &root).unwrap();
        assert!(
            state
                .install_authenticator(Signed::sign(record.clone(), &node).unwrap(), NOW)
                .is_err()
        );
        assert!(state.verify_authenticator(&signed, NOW).is_err());
        let hash = state.install_authenticator(signed.clone(), NOW).unwrap();
        document.previous = Some(initial);
        document.sequence = 1;
        document.authenticators = vec![hash];
        state
            .install_account(Signed::sign(document.clone(), &root).unwrap())
            .unwrap();
        let authorized = state.verify_authenticator(&signed, NOW).unwrap();
        proof.matches_authorized_record(&authorized).unwrap();
        let passkey = RegisteredPasskey::from_authorized(
            &authorized,
            AuthenticatorState {
                sign_count: proof.sign_count(),
                backup_state: proof.backup_state(),
            },
        )
        .unwrap();
        let assertion_pending = pending(
            PasskeyPurpose::EnrollDevice,
            AuthenticationCeremony::IdentifiedAccount,
        );
        let response = assertion(
            &assertion_pending,
            auth(&assertion_pending, 0x0d, 0),
            client(&assertion_pending, "webauthn.get"),
            None,
        );
        assert!(verify_passkey_assertion(&assertion_pending, &passkey, &response, NOW).is_ok());
        let journal = vec![
            JournalEntry::AccountGenesis(genesis),
            JournalEntry::Authenticator(signed.clone()),
            JournalEntry::Account(Signed::sign(document.clone(), &root).unwrap()),
        ];
        let pins = vec![PolicyPin {
            domain: PolicyDomain::Account(account.clone()),
            genesis_root: PublicKey::of(&root),
            minimum_head: state.head(&PolicyDomain::Account(account.clone())).cloned(),
        }];
        let proof = PasskeyAssertionProof {
            pending: assertion_pending.clone(),
            record: signed.clone(),
            state: passkey.state(),
            response,
        };
        let expected = assertion_pending.envelope_hash().unwrap();
        let proof_json = serde_json::to_string(&proof).unwrap();
        let journal_json = serde_json::to_string(&journal).unwrap();
        let pins_json = serde_json::to_string(&pins).unwrap();
        let verified = verify_assertion_json(
            &proof_json,
            &serde_json::to_string(&proof.state).unwrap(),
            &pins_json,
            &journal_json,
            expected.as_str(),
            "client.example",
            "https://client.example",
            NOW as f64,
        )
        .unwrap();
        assert_eq!(verified.account(), &account);
        assert!(verified.proof_json().unwrap().contains("record_hash"));
        let mut lowered = proof.clone();
        lowered.state.sign_count = 2;
        lowered.response = assertion(
            &assertion_pending,
            auth(&assertion_pending, 0x0d, 3),
            client(&assertion_pending, "webauthn.get"),
            None,
        );
        let lowered_json = serde_json::to_string(&lowered).unwrap();
        let trusted = AuthenticatorState {
            sign_count: 3,
            backup_state: false,
        };
        assert!(
            verify_assertion_json(
                &lowered_json,
                &serde_json::to_string(&trusted).unwrap(),
                &pins_json,
                &journal_json,
                expected.as_str(),
                "client.example",
                "https://client.example",
                NOW as f64
            )
            .is_err()
        );
        assert!(
            verify_assertion_json(
                &lowered_json,
                &serde_json::to_string(&lowered.state).unwrap(),
                &pins_json,
                &journal_json,
                expected.as_str(),
                "client.example",
                "https://client.example",
                NOW as f64
            )
            .is_ok()
        );
        let mut wrong_pins = pins.clone();
        wrong_pins[0].genesis_root = PublicKey::of(&node);
        assert!(
            verify_assertion_json(
                &proof_json,
                &serde_json::to_string(&proof.state).unwrap(),
                &serde_json::to_string(&wrong_pins).unwrap(),
                &journal_json,
                expected.as_str(),
                "client.example",
                "https://client.example",
                NOW as f64
            )
            .is_err()
        );
        let mut other = registration_pending.clone();
        other.envelope.enrollment_hash = ObjectHash::body(&"different approved request").unwrap();
        let other_proof = verify_passkey_registration(&other, &registration(&other), NOW).unwrap();
        assert!(other_proof.matches_authorized_record(&authorized).is_err());
        document.previous = state.head(&PolicyDomain::Account(account)).cloned();
        document.sequence += 1;
        document.authenticators.clear();
        state
            .install_account(Signed::sign(document, &root).unwrap())
            .unwrap();
        assert!(state.verify_authenticator(&signed, NOW).is_err());
    }
    #[cfg_attr(not(target_arch = "wasm32"), test)]
    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    fn hostile_nested_encodings_return_errors_in_both_runtimes() {
        let p = pending(
            PasskeyPurpose::EnrollDevice,
            AuthenticationCeremony::IdentifiedAccount,
        );
        let credential = registered(&p);
        let mut data = auth(&p, 0x8d, 0);
        data.extend_from_slice(b"\xa1\x66nested");
        data.extend(std::iter::repeat_n(0x81, 1024));
        data.push(0xf6);
        let response = assertion(&p, data, client(&p, "webauthn.get"), None);
        assert!(verify_passkey_assertion(&p, &credential, &response, NOW).is_err());
        let p = pending(
            PasskeyPurpose::RegisterAuthenticator,
            AuthenticationCeremony::IdentifiedAccount,
        );
        let response = registration(&p);
        let mut value = serde_json::to_value(&response).unwrap();
        value
            .as_object_mut()
            .unwrap()
            .remove("clientExtensionResults");
        let mut json = serde_json::to_string(&value).unwrap();
        json.pop();
        let json = format!(
            "{json},\"clientExtensionResults\":{}null{}}}",
            "[".repeat(1024),
            "]".repeat(1024)
        );
        assert!(
            serde_json::from_str::<RegistrationResponse>(&json)
                .unwrap_err()
                .to_string()
                .contains("recursion limit")
        );
        assert!(
            verify_registration_json(
                &serde_json::to_string(&p).unwrap(),
                &json,
                p.envelope_hash().unwrap().as_str(),
                "client.example",
                "https://client.example",
                NOW as f64
            )
            .is_err()
        );
        let mut raw = vec![0xa1, 0x66];
        raw.extend_from_slice(b"nested");
        raw.extend(std::iter::repeat_n(0x81, 1024));
        raw.push(0xf6);
        let mut response = response;
        response.response.attestation_object = URL_SAFE_NO_PAD.encode(raw);
        assert!(verify_passkey_registration(&p, &response, NOW).is_err());
    }
}
