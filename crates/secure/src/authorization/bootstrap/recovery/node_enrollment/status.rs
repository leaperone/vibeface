use super::*;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AccountNodeEnrollmentStatusPurpose {
    AccountNodeEnrollmentStatus,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct AccountNodeEnrollmentStatusRequest {
    pub version: u16,
    pub purpose: AccountNodeEnrollmentStatusPurpose,
    pub account: AccountId,
    pub requesting_device: DeviceId,
    pub source_node_public_key: PublicKey,
    pub target_node_public_key: PublicKey,
    pub enrollment_intent_hash: ObjectHash,
    pub commit_input_hash: ObjectHash,
    pub nonce: Nonce,
    pub issued_at_ms: u64,
    pub expires_at_ms: u64,
}
impl sealed::Sealed for AccountNodeEnrollmentStatusRequest {}
impl SignedObject for AccountNodeEnrollmentStatusRequest {
    const DOMAIN: &'static str = "account-node-enrollment-status-request-v2";
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
pub enum AccountNodeEnrollmentStatusResult {
    NotObserved,
    Committed { journal_delta: Vec<JournalEntry> },
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct AccountNodeEnrollmentStatusResponse {
    pub version: u16,
    pub purpose: AccountNodeEnrollmentStatusPurpose,
    pub request_hash: ObjectHash,
    pub account: AccountId,
    pub requesting_device: DeviceId,
    pub source_node_public_key: PublicKey,
    pub target_node_public_key: PublicKey,
    pub enrollment_intent_hash: ObjectHash,
    pub commit_input_hash: ObjectHash,
    pub result: AccountNodeEnrollmentStatusResult,
    pub issued_at_ms: u64,
    pub expires_at_ms: u64,
}
impl sealed::Sealed for AccountNodeEnrollmentStatusResponse {}
impl SignedObject for AccountNodeEnrollmentStatusResponse {
    const DOMAIN: &'static str = "account-node-enrollment-status-response-v2";
}

#[derive(Clone, Debug)]
#[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen)]
pub struct PreparedAccountNodeEnrollmentStatus {
    request: Signed<AccountNodeEnrollmentStatusRequest>,
    enrollment: PreparedAccountNodeEnrollmentCommit,
}

#[derive(Clone, Debug)]
#[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen)]
pub struct VerifiedAccountNodeEnrollmentStatus {
    enrollment: Option<VerifiedAccountNodeEnrollment>,
}

fn window(issued: u64, expires: u64, now: u64) -> Result<()> {
    validate_window(issued, expires, now)?;
    require(
        expires - issued <= NODE_CLAIM_MAX_AGE_MS,
        "node enrollment status query lifetime is too long",
    )
}

pub fn verify_account_node_enrollment_status_request(
    state: &VerifiedPolicySet,
    request: &Signed<AccountNodeEnrollmentStatusRequest>,
    local: &PublicKey,
    noise: &PublicKey,
    now: u64,
) -> Result<ExecutionProofUsage> {
    let _scope = crate::signature_verification::ScopedSignatureVerification::new();
    require(
        canonical(request)?.len() <= MAX_PROOF_BYTES,
        "node enrollment status request exceeds its carrier limit",
    )?;
    request.verify_signature()?;
    let value = &request.payload;
    window(value.issued_at_ms, value.expires_at_ms, now)?;
    let account = state
        .account(&value.account)
        .ok_or(AuthorizationError::Revoked)?;
    require(
        value.version == AUTH_VERSION
            && value.purpose == AccountNodeEnrollmentStatusPurpose::AccountNodeEnrollmentStatus
            && request.issuer == *noise
            && value.requesting_device == DeviceId::from_key(noise)
            && value.source_node_public_key == *local
            && account.authority.public_key == *local,
        "node enrollment status belongs to another account authority or Noise client",
    )?;
    let client =
        state.verify_account_client_device(&value.account, &value.requesting_device, now)?;
    require(
        client.payload.public_key == *noise
            && client.payload.uses.contains(&DeviceUse::EnrollDevice)
            && account.enrollment.iter().any(|authority| {
                authority.device == value.requesting_device
                    && now < authority.expires_at_ms
                    && authority.rights.contains(&AccountRight::EnrollDevice)
            }),
        "node enrollment status requires current account enrollment rights",
    )?;
    let source = state.verify_transport_device(&DeviceId::from_key(local), now)?;
    let mut budget = Budget::default();
    for certificate in [client, source] {
        state.validate_certificate(
            certificate,
            now,
            &mut budget,
            &mut BTreeMap::new(),
            &mut BTreeSet::new(),
        )?;
    }
    let mut usage = ExecutionProofUsage::default();
    usage.record_signed(request)?;
    usage.record_budget(&budget)?;
    usage.enforce_limits()?;
    Ok(usage)
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen(js_name = prepareAccountNodeEnrollmentStatus))]
pub fn prepare_account_node_enrollment_status(
    enrollment: &PreparedAccountNodeEnrollmentCommit,
    client: &Identity,
    now_ms: f64,
    expires_at_ms: f64,
) -> crate::Result<PreparedAccountNodeEnrollmentStatus> {
    let _scope = crate::signature_verification::ScopedSignatureVerification::new();
    let prepare = || -> Result<_> {
        let now = timestamp(now_ms)?;
        let expires = timestamp(expires_at_ms)?;
        window(now, expires, now)?;
        let original = &enrollment.request.intent.payload;
        require(
            original.requesting_device == DeviceId::from_key(&PublicKey::of(client)),
            "node enrollment status belongs to another requesting client",
        )?;
        let request = Signed::sign(
            AccountNodeEnrollmentStatusRequest {
                version: AUTH_VERSION,
                purpose: AccountNodeEnrollmentStatusPurpose::AccountNodeEnrollmentStatus,
                account: original.account.clone(),
                requesting_device: original.requesting_device.clone(),
                source_node_public_key: enrollment
                    .request
                    .descriptor
                    .source_node_public_key
                    .clone(),
                target_node_public_key: original.claim.payload.node_public_key.clone(),
                enrollment_intent_hash: ObjectHash::body(&enrollment.request.intent)?,
                commit_input_hash: ObjectHash::body(&enrollment.request)?,
                nonce: Nonce::random()?,
                issued_at_ms: now,
                expires_at_ms: expires,
            },
            client,
        )?;
        Ok(PreparedAccountNodeEnrollmentStatus {
            request,
            enrollment: enrollment.clone(),
        })
    };
    prepare().map_err(|error| error.to_string())
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen)]
impl PreparedAccountNodeEnrollmentStatus {
    #[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen(js_name = requestJson))]
    pub fn request_json(&self) -> crate::Result<String> {
        encode(&serde_json::json!({"request":self.request}))
    }
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen(js_name = verifyAccountNodeEnrollmentStatus))]
pub fn verify_account_node_enrollment_status(
    prepared: &PreparedAccountNodeEnrollmentStatus,
    response_json: &str,
    now_ms: f64,
) -> crate::Result<VerifiedAccountNodeEnrollmentStatus> {
    let _scope = crate::signature_verification::ScopedSignatureVerification::new();
    let verify = || -> Result<_> {
        let response: Signed<AccountNodeEnrollmentStatusResponse> =
            decode_bounded(response_json, MAX_PROOF_BYTES)?;
        let now = timestamp(now_ms)?;
        let request = &prepared.request.payload;
        let value = &response.payload;
        window(request.issued_at_ms, request.expires_at_ms, now)?;
        window(value.issued_at_ms, value.expires_at_ms, now)?;
        require(
            response.issuer == request.source_node_public_key
                && value.version == AUTH_VERSION
                && value.purpose == request.purpose
                && value.request_hash == prepared.request.hash()?
                && value.account == request.account
                && value.requesting_device == request.requesting_device
                && value.source_node_public_key == request.source_node_public_key
                && value.target_node_public_key == request.target_node_public_key
                && value.enrollment_intent_hash == request.enrollment_intent_hash
                && value.commit_input_hash == request.commit_input_hash
                && request.issued_at_ms <= value.issued_at_ms
                && value.expires_at_ms <= request.expires_at_ms,
            "node enrollment status response changed its source, original operation or query",
        )?;
        response.verify_signature()?;
        let mut usage = ExecutionProofUsage::default();
        usage.record_signed(&prepared.request)?;
        usage.record_signed(&response)?;
        usage.record_metadata(&serde_json::json!({"request":prepared.request}))?;
        usage.record_metadata(&serde_json::json!({"status":response}))?;
        let enrollment = match &value.result {
            AccountNodeEnrollmentStatusResult::NotObserved => None,
            AccountNodeEnrollmentStatusResult::Committed { journal_delta } => {
                for entry in journal_delta {
                    usage.merge(
                        &crate::authorization::policy_proof::policy_entry_payload_usage(entry)?,
                    );
                }
                let [
                    JournalEntry::Device(_),
                    JournalEntry::AccountCommit(committed),
                ] = journal_delta.as_slice()
                else {
                    return Err(AuthorizationError::InvalidProof(
                        "node enrollment status requires its exact committed delta",
                    ));
                };
                let original = &prepared.enrollment.request;
                let requester = prepared
                    .enrollment
                    .snapshot
                    .state()
                    .find_device(&original.intent.payload.requesting_device)?;
                usage.merge(&super::verify_account_node_enrollment_read(
                    prepared.enrollment.snapshot.state(),
                    &original.intent,
                    &original.descriptor,
                    &original.descriptor.source_node_public_key,
                    &requester.payload.public_key,
                    committed.commit.payload.accepted_at_ms,
                )?);
                let finished = super::finish(&prepared.enrollment, journal_delta.clone(), now)?;
                if let Some(authorization) = &finished.authorization {
                    usage.record_budget(&authorization.budget)?;
                }
                Some(finished)
            }
        };
        usage.enforce_limits()?;
        require(
            canonical(&serde_json::json!({"request":prepared.request}))?
                .len()
                .checked_add(canonical(&serde_json::json!({"status":response}))?.len())
                .is_some_and(|bytes| bytes <= MAX_PROOF_BYTES),
            "node enrollment status exchange exceeds its carrier limit",
        )?;
        Ok(VerifiedAccountNodeEnrollmentStatus { enrollment })
    };
    verify().map_err(|error| error.to_string())
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen)]
impl VerifiedAccountNodeEnrollmentStatus {
    pub fn state(&self) -> String {
        if self.enrollment.is_some() {
            "committed"
        } else {
            "not_observed"
        }
        .to_owned()
    }
    pub fn enrollment(&self) -> Option<VerifiedAccountNodeEnrollment> {
        self.enrollment.clone()
    }
}
