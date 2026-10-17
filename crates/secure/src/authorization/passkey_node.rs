//! Node signatures bind browser challenges and current SQL credential state.

use super::*;
use crate::webauthn::{
    AuthenticatorState, PASSKEY_MAX_AGE_MS, PasskeyAssertionProof, PasskeyPurpose, PendingPasskey,
};

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct NodePasskeyPending {
    pub version: u16,
    pub pending: PendingPasskey,
    pub issued_at_ms: u64,
    pub expires_at_ms: u64,
}
impl sealed::Sealed for NodePasskeyPending {}
impl SignedObject for NodePasskeyPending {
    const DOMAIN: &'static str = "node-passkey-pending-v2";
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct NodePasskeyState {
    pub version: u16,
    pub pending: PendingPasskey,
    pub credential_id: String,
    pub record_hash: ObjectHash,
    pub state: AuthenticatorState,
    pub response_hash: ObjectHash,
    pub issued_at_ms: u64,
    pub expires_at_ms: u64,
}
impl sealed::Sealed for NodePasskeyState {}
impl SignedObject for NodePasskeyState {
    const DOMAIN: &'static str = "node-passkey-state-v2";
}

#[derive(Clone, Debug)]
#[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen)]
pub struct VerifiedNodePasskeyPending {
    node: PublicKey,
    signed: Signed<NodePasskeyPending>,
}

#[derive(Clone, Debug)]
#[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen)]
pub struct VerifiedNodePasskeyState {
    state: AuthenticatorState,
}

pub fn commit_passkey_policy_at_authority(
    state: &VerifiedPolicySet,
    proposal: Signed<AccountDocument>,
    authority: &Identity,
    not_after_ms: u64,
    now_ms: u64,
) -> Result<Committed<AccountDocument>> {
    let account = state
        .account(&proposal.payload.account)
        .ok_or(AuthorizationError::PolicyConflict)?;
    let authority_key = PublicKey::of(authority);
    require(
        account.authority.public_key == authority_key,
        "passkey commit requires the current local account authority",
    )?;
    let domain = PolicyDomain::Account(account.account.clone());
    let mut budget = Budget::default();
    if proposal.issuer != account.current_root && !account.recovery.keys.contains(&proposal.issuer)
    {
        state.authorize_account_delta(&proposal, account, now_ms, &mut budget)?;
    }
    let mut dependencies = budget.dependencies.clone();
    state.dependency(&domain, &mut dependencies)?;
    state.expand_authorities(now_ms, &mut budget, &mut dependencies)?;
    let dependencies: Vec<_> = dependencies.into_values().collect();
    for dependency in &dependencies {
        let current = match &dependency.domain {
            PolicyDomain::Account(id) => state.account(id).map(|d| &d.authority),
            PolicyDomain::Space(id) => state.space(id).map(|d| &d.authority),
        }
        .ok_or(AuthorizationError::PolicyConflict)?;
        require(
            current.public_key == authority_key,
            "passkey completion cannot issue another authority's freshness receipt",
        )?;
    }
    let expires = now_ms.saturating_add(EXECUTION_LEASE_MS).min(not_after_ms);
    require(now_ms < expires, "passkey authority completion expired")?;
    let commit = Signed::sign(
        PolicyCommit {
            version: AUTH_VERSION,
            previous: state
                .head(&domain)
                .cloned()
                .ok_or(AuthorizationError::PolicyConflict)?,
            proposal_hash: proposal.hash()?,
            accepted_at_ms: now_ms,
            issued_at_ms: now_ms,
            expires_at_ms: expires,
            nonce: Nonce::random()?,
            dependencies: dependencies.clone(),
        },
        authority,
    )?;
    let challenge_hash = commit.challenge_hash()?;
    let receipts = dependencies
        .into_iter()
        .map(|current| {
            Signed::sign(
                FreshnessReceipt {
                    version: AUTH_VERSION,
                    current,
                    challenge_hash: challenge_hash.clone(),
                    issued_at_ms: now_ms,
                    expires_at_ms: expires,
                },
                authority,
            )
        })
        .collect::<Result<Vec<_>>>()?;
    let committed = Committed {
        proposal,
        commit,
        receipts,
    };
    let mut checked = state.clone();
    checked.install_account_committed(committed.clone(), now_ms)?;
    Ok(committed)
}

fn parse<T: for<'de> Deserialize<'de>>(json: &str) -> Result<T> {
    require(json.len() <= MAX_PROOF_BYTES, "node passkey JSON too large")?;
    serde_json::from_str(json)
        .map_err(|_| AuthorizationError::InvalidProof("invalid node passkey JSON"))
}

fn signed_window(pending: &PendingPasskey, issued: u64, expires: u64, now: u64) -> Result<()> {
    let envelope = pending.envelope();
    require(
        envelope.version == AUTH_VERSION
            && envelope.not_before_ms <= issued
            && issued <= now
            && now < expires
            && expires <= envelope.expires_at_ms
            && expires.saturating_sub(issued) <= PASSKEY_MAX_AGE_MS,
        "node passkey statement expired or outside its pending lifetime",
    )?;
    pending
        .validate(envelope.purpose.clone(), now)
        .map_err(|_| AuthorizationError::InvalidProof("invalid node passkey pending"))
}

fn intent_binding(pending: &PendingPasskey, expected_json: &str) -> Result<()> {
    let envelope = pending.envelope();
    match envelope.purpose {
        PasskeyPurpose::RegisterAuthenticator => {
            let signed: Signed<enrollment::AuthenticatorEnrollmentIntent> = parse(expected_json)?;
            signed.verify_signature()?;
            let intent = &signed.payload;
            require(
                envelope.account == intent.account
                    && envelope.device_public_key == intent.requesting_device_public_key
                    && envelope.rights == [AccountRight::ManageAuthenticators]
                    && envelope.target == intent.target
                    && envelope.enrollment_hash == ObjectHash::body(intent)?
                    && envelope.expected_heads == intent.expected_heads
                    && envelope.not_before_ms >= intent.credential_not_before_ms
                    && envelope.expires_at_ms <= intent.credential_expires_at_ms,
                "node registration pending changed its signed intent",
            )
        }
        PasskeyPurpose::EnrollDevice => {
            let signed: Signed<enrollment::DeviceEnrollmentIntent> = parse(expected_json)?;
            signed.verify_signature()?;
            let intent = &signed.payload;
            require(
                envelope.account == intent.account
                    && envelope.device_public_key == intent.new_device_public_key
                    && envelope.rights == intent.requested_rights
                    && envelope.target == intent.target
                    && envelope.enrollment_hash == ObjectHash::body(intent)?
                    && envelope.expected_heads == intent.expected_heads
                    && envelope.not_before_ms >= intent.certificate_not_before_ms
                    && envelope.expires_at_ms <= intent.certificate_expires_at_ms,
                "node device pending changed its signed intent",
            )
        }
        PasskeyPurpose::ManageAuthenticators => {
            let signed: Signed<AuthenticatorManagementIntent> = parse(expected_json)?;
            signed.verify_signature()?;
            let intent = &signed.payload;
            require(
                envelope.account == intent.account
                    && envelope.device_public_key == intent.device_public_key
                    && envelope.rights == [AccountRight::ManageAuthenticators]
                    && envelope.target == intent.target
                    && envelope.enrollment_hash == ObjectHash::body(intent)?
                    && envelope.expected_heads == intent.expected_heads
                    && envelope.not_before_ms >= intent.issued_at_ms
                    && envelope.expires_at_ms <= intent.expires_at_ms,
                "node management pending changed its signed intent",
            )
        }
    }
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen(js_name = verifyNodePasskeyPending))]
pub fn verify_node_passkey_pending(
    signed_json: &str,
    expected_node_public_key: &str,
    expected_signed_intent_json: &str,
    expected_rp_id: &str,
    expected_origin: &str,
    now_ms: f64,
) -> crate::Result<VerifiedNodePasskeyPending> {
    let verify = || -> Result<VerifiedNodePasskeyPending> {
        let now = bootstrap::timestamp(now_ms)?;
        let node = PublicKey::parse(expected_node_public_key)?;
        let signed: Signed<NodePasskeyPending> = parse(signed_json)?;
        require(
            signed.issuer == node
                && signed.payload.version == AUTH_VERSION
                && signed.payload.pending.envelope().target == DeviceId::from_key(&node),
            "passkey pending was not signed by the pinned node",
        )?;
        signed.verify_signature()?;
        let pending = &signed.payload.pending;
        signed_window(
            pending,
            signed.payload.issued_at_ms,
            signed.payload.expires_at_ms,
            now,
        )?;
        require(
            pending.relying_party().rp_id() == expected_rp_id
                && pending.relying_party().origin() == expected_origin,
            "node passkey pending changed its trusted RP or origin",
        )?;
        intent_binding(pending, expected_signed_intent_json)?;
        Ok(VerifiedNodePasskeyPending { node, signed })
    };
    verify().map_err(|e| e.to_string())
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen(js_name = verifyNodePasskeyState))]
pub fn verify_node_passkey_state(
    signed_json: &str,
    trusted_pending: &VerifiedNodePasskeyPending,
    proof_json: &str,
    expected_browser_response_json: &str,
    now_ms: f64,
) -> crate::Result<VerifiedNodePasskeyState> {
    let verify = || -> Result<VerifiedNodePasskeyState> {
        let now = bootstrap::timestamp(now_ms)?;
        let signed: Signed<NodePasskeyState> = parse(signed_json)?;
        let statement = &signed.payload;
        require(
            signed.issuer == trusted_pending.node
                && statement.version == AUTH_VERSION
                && statement.pending == trusted_pending.signed.payload.pending,
            "passkey state was not signed by the original pinned node",
        )?;
        signed.verify_signature()?;
        signed_window(
            &statement.pending,
            statement.issued_at_ms,
            statement.expires_at_ms,
            now,
        )?;
        require(
            statement.issued_at_ms >= trusted_pending.signed.payload.issued_at_ms
                && statement.expires_at_ms <= trusted_pending.signed.payload.expires_at_ms,
            "passkey state changed its original challenge lifetime",
        )?;
        let proof: PasskeyAssertionProof = parse(proof_json)?;
        let expected: crate::webauthn::AssertionResponse = parse(expected_browser_response_json)?;
        require(
            proof.pending == statement.pending
                && proof.response == expected
                && proof.record.hash()? == statement.record_hash
                && proof.record.payload.credential_id == statement.credential_id
                && proof.response.raw_id == statement.credential_id
                && proof.state == statement.state
                && ObjectHash::body(&proof.response)? == statement.response_hash,
            "passkey state does not bind the exact proof and current counter",
        )?;
        Ok(VerifiedNodePasskeyState {
            state: statement.state,
        })
    };
    verify().map_err(|e| e.to_string())
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen)]
impl VerifiedNodePasskeyPending {
    #[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen(js_name = pendingJson))]
    pub fn pending_json(&self) -> crate::Result<String> {
        serde_json::to_string(&self.signed.payload.pending)
            .map_err(|_| "node passkey pending encoding failed".to_owned())
    }
    #[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen(js_name = envelopeHash))]
    pub fn envelope_hash(&self) -> crate::Result<String> {
        self.signed
            .payload
            .pending
            .envelope_hash()
            .map(|hash| hash.to_string())
    }
    #[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen(js_name = optionsJson))]
    pub fn options_json(&self) -> crate::Result<String> {
        let pending = &self.signed.payload.pending;
        let options = match pending.envelope().purpose {
            PasskeyPurpose::RegisterAuthenticator => pending.creation_options(),
            PasskeyPurpose::EnrollDevice | PasskeyPurpose::ManageAuthenticators => {
                pending.authentication_options()
            }
        }?;
        serde_json::to_string(&options)
            .map_err(|_| "node passkey options encoding failed".to_owned())
    }
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen)]
impl VerifiedNodePasskeyState {
    #[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen(js_name = trustedStateJson))]
    pub fn trusted_state_json(&self) -> crate::Result<String> {
        serde_json::to_string(&self.state)
            .map_err(|_| "node passkey state encoding failed".to_owned())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::webauthn::{AuthenticationCeremony, EnrollmentEnvelope, RelyingParty};
    use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};

    const NOW: u64 = 1000;

    fn key(byte: u8) -> Identity {
        Identity::from_secret_hex(&hex::encode([byte; 32])).unwrap()
    }
    fn json<T: Serialize>(value: &T) -> String {
        serde_json::to_string(value).unwrap()
    }

    struct Fixture {
        node: Identity,
        device: Identity,
        intent: Signed<enrollment::DeviceEnrollmentIntent>,
        pending: Signed<NodePasskeyPending>,
        proof: PasskeyAssertionProof,
        state: Signed<NodePasskeyState>,
    }
    impl Fixture {
        fn new() -> Self {
            let root = key(1);
            let node = key(2);
            let device = key(4);
            let account = AccountId::from_root(&PublicKey::of(&root));
            let head = PolicyStamp {
                domain: PolicyDomain::Account(account.clone()),
                authority_epoch: 0,
                sequence: 1,
                head: ObjectHash::body(&"current account policy").unwrap(),
            };
            let intent = Signed::sign(
                enrollment::DeviceEnrollmentIntent {
                    account: account.clone(),
                    new_device_public_key: PublicKey::of(&device),
                    requested_rights: vec![],
                    target: DeviceId::from_key(&PublicKey::of(&node)),
                    certificate_not_before_ms: NOW,
                    certificate_expires_at_ms: NOW + 100_000,
                    expected_heads: vec![head.clone()],
                },
                &device,
            )
            .unwrap();
            let pending = PendingPasskey::prepare(
                EnrollmentEnvelope {
                    version: AUTH_VERSION,
                    purpose: PasskeyPurpose::EnrollDevice,
                    account: account.clone(),
                    device_public_key: PublicKey::of(&device),
                    rights: vec![],
                    target: intent.payload.target.clone(),
                    enrollment_hash: ObjectHash::body(&intent.payload).unwrap(),
                    expected_heads: vec![head.clone()],
                    nonce: Nonce::from_bytes([0; 32]),
                    not_before_ms: NOW,
                    expires_at_ms: NOW + 100_000,
                },
                RelyingParty::new("client.example", "https://client.example").unwrap(),
                &[7; 32],
                AuthenticationCeremony::Discoverable,
                vec![],
                NOW,
            )
            .unwrap();
            let mut registration_envelope = pending.envelope().clone();
            registration_envelope.purpose = PasskeyPurpose::RegisterAuthenticator;
            registration_envelope.rights = vec![AccountRight::ManageAuthenticators];
            registration_envelope.enrollment_hash =
                ObjectHash::body(&"registration intent").unwrap();
            let registration_pending = PendingPasskey::prepare(
                registration_envelope,
                pending.relying_party().clone(),
                &[7; 32],
                AuthenticationCeremony::IdentifiedAccount,
                vec![],
                NOW,
            )
            .unwrap();
            let registration = crate::webauthn::verify_passkey_registration(
                &registration_pending,
                &crate::webauthn::tests::registration(&registration_pending),
                NOW,
            )
            .unwrap();
            let record = Signed::sign(
                AuthenticatorRecord {
                    version: AUTH_VERSION,
                    account,
                    credential_id: URL_SAFE_NO_PAD.encode([9; 32]),
                    cose_public_key: registration.cose_public_key().to_vec(),
                    rp_id: "client.example".into(),
                    origins: vec!["https://client.example".into()],
                    user_handle: pending.user_handle().to_owned(),
                    backup_eligible: true,
                    client_enrollment: PasskeyClientEnrollment::AccountClient,
                    initial_state: AuthenticatorState {
                        sign_count: registration.sign_count(),
                        backup_state: registration.backup_state(),
                    },
                    issued_under: head,
                    nonce: registration_pending.envelope().nonce.clone(),
                    enrollment_hash: registration_pending.envelope_hash().unwrap(),
                    not_before_ms: NOW,
                    expires_at_ms: NOW + 500_000,
                },
                &root,
            )
            .unwrap();
            let state = AuthenticatorState {
                sign_count: 1,
                backup_state: false,
            };
            let response = crate::webauthn::tests::assertion(
                &pending,
                crate::webauthn::tests::auth(&pending, 0x0d, 2),
                crate::webauthn::tests::client(&pending, "webauthn.get"),
                Some(pending.user_handle().to_owned()),
            );
            let proof = PasskeyAssertionProof {
                pending: pending.clone(),
                record,
                state,
                response,
            };
            let statement = Signed::sign(
                NodePasskeyState {
                    version: AUTH_VERSION,
                    pending: pending.clone(),
                    credential_id: proof.record.payload.credential_id.clone(),
                    record_hash: proof.record.hash().unwrap(),
                    state,
                    response_hash: ObjectHash::body(&proof.response).unwrap(),
                    issued_at_ms: NOW,
                    expires_at_ms: NOW + 100_000,
                },
                &node,
            )
            .unwrap();
            let pending = Signed::sign(
                NodePasskeyPending {
                    version: AUTH_VERSION,
                    pending,
                    issued_at_ms: NOW,
                    expires_at_ms: NOW + 100_000,
                },
                &node,
            )
            .unwrap();
            Self {
                node,
                device,
                intent,
                pending,
                proof,
                state: statement,
            }
        }
        fn verify_pending(
            &self,
            signed: &Signed<NodePasskeyPending>,
        ) -> crate::Result<VerifiedNodePasskeyPending> {
            verify_node_passkey_pending(
                &json(signed),
                PublicKey::of(&self.node).as_str(),
                &json(&self.intent),
                "client.example",
                "https://client.example",
                NOW as f64,
            )
        }
    }

    #[cfg_attr(not(target_arch = "wasm32"), test)]
    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    fn pinned_node_pending_binds_the_complete_intent_and_generates_browser_options() {
        let f = Fixture::new();
        let verified = f.verify_pending(&f.pending).unwrap();
        let options: serde_json::Value =
            serde_json::from_str(&verified.options_json().unwrap()).unwrap();
        assert_eq!(
            options["publicKey"]["allowCredentials"],
            serde_json::json!([])
        );
        assert_eq!(
            verified.envelope_hash().unwrap(),
            f.proof.pending.envelope_hash().unwrap().to_string()
        );
        assert_eq!(verified.pending_json().unwrap(), json(&f.proof.pending));
        assert!(
            verify_node_passkey_pending(
                &json(&f.pending),
                PublicKey::of(&key(8)).as_str(),
                &json(&f.intent),
                "client.example",
                "https://client.example",
                NOW as f64
            )
            .is_err()
        );
        assert!(
            verify_node_passkey_pending(
                &json(&f.pending),
                PublicKey::of(&f.node).as_str(),
                &json(&f.intent),
                "client.example",
                "https://other.example",
                NOW as f64
            )
            .is_err()
        );
        let mut changed = f.intent.payload.clone();
        changed.certificate_expires_at_ms -= 1;
        let changed = Signed::sign(changed, &f.device).unwrap();
        assert!(
            verify_node_passkey_pending(
                &json(&f.pending),
                PublicKey::of(&f.node).as_str(),
                &json(&changed),
                "client.example",
                "https://client.example",
                NOW as f64
            )
            .is_err()
        );
        let mut changed = f.pending.clone();
        changed.payload.expires_at_ms -= 1;
        assert!(f.verify_pending(&changed).is_err());
        assert!(
            verify_node_passkey_pending(
                &json(&f.pending),
                PublicKey::of(&f.node).as_str(),
                &json(&f.intent),
                "client.example",
                "https://client.example",
                (NOW + 100_000) as f64
            )
            .is_err()
        );
    }

    #[cfg_attr(not(target_arch = "wasm32"), test)]
    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    fn signed_node_state_rejects_replaced_counters_records_responses_and_challenges() {
        let f = Fixture::new();
        let trusted = f.verify_pending(&f.pending).unwrap();
        let verified = verify_node_passkey_state(
            &json(&f.state),
            &trusted,
            &json(&f.proof),
            &json(&f.proof.response),
            NOW as f64,
        )
        .unwrap();
        assert_eq!(
            verified.trusted_state_json().unwrap(),
            "{\"sign_count\":1,\"backup_state\":false}"
        );
        let another_response = crate::webauthn::tests::assertion(
            &f.proof.pending,
            crate::webauthn::tests::auth(&f.proof.pending, 0x0d, 3),
            crate::webauthn::tests::client(&f.proof.pending, "webauthn.get"),
            Some(f.proof.pending.user_handle().to_owned()),
        );
        assert!(
            verify_node_passkey_state(
                &json(&f.state),
                &trusted,
                &json(&f.proof),
                &json(&another_response),
                NOW as f64,
            )
            .is_err()
        );
        let mut substituted_proof = f.proof.clone();
        substituted_proof.response = another_response;
        let mut substituted_state = f.state.payload.clone();
        substituted_state.response_hash = ObjectHash::body(&substituted_proof.response).unwrap();
        let substituted_state = Signed::sign(substituted_state, &f.node).unwrap();
        assert!(
            verify_node_passkey_state(
                &json(&substituted_state),
                &trusted,
                &json(&substituted_proof),
                &json(&f.proof.response),
                NOW as f64,
            )
            .is_err()
        );
        let mut changed = f.proof.clone();
        changed.state.sign_count = 0;
        assert!(
            verify_node_passkey_state(
                &json(&f.state),
                &trusted,
                &json(&changed),
                &json(&f.proof.response),
                NOW as f64
            )
            .is_err()
        );
        let mut changed = f.proof.clone();
        changed.response.client_extension_results = serde_json::json!({"another":true});
        assert!(
            verify_node_passkey_state(
                &json(&f.state),
                &trusted,
                &json(&changed),
                &json(&f.proof.response),
                NOW as f64
            )
            .is_err()
        );
        let mut changed = f.proof.clone();
        changed.record.payload.expires_at_ms -= 1;
        assert!(
            verify_node_passkey_state(
                &json(&f.state),
                &trusted,
                &json(&changed),
                &json(&f.proof.response),
                NOW as f64
            )
            .is_err()
        );
        let mut changed = f.state.payload.clone();
        changed.record_hash = ObjectHash::body(&"different record").unwrap();
        let changed = Signed::sign(changed, &f.node).unwrap();
        assert!(
            verify_node_passkey_state(
                &json(&changed),
                &trusted,
                &json(&f.proof),
                &json(&f.proof.response),
                NOW as f64
            )
            .is_err()
        );
        let changed = Signed::sign(f.state.payload.clone(), &key(8)).unwrap();
        assert!(
            verify_node_passkey_state(
                &json(&changed),
                &trusted,
                &json(&f.proof),
                &json(&f.proof.response),
                NOW as f64
            )
            .is_err()
        );
        let mut changed = f.state.clone();
        changed.payload.state.sign_count = 0;
        assert!(
            verify_node_passkey_state(
                &json(&changed),
                &trusted,
                &json(&f.proof),
                &json(&f.proof.response),
                NOW as f64
            )
            .is_err()
        );
        let other = Fixture::new();
        let other_pending = other.verify_pending(&other.pending).unwrap();
        assert!(
            verify_node_passkey_state(
                &json(&f.state),
                &other_pending,
                &json(&f.proof),
                &json(&f.proof.response),
                NOW as f64
            )
            .is_err()
        );
        assert!(
            verify_node_passkey_state(
                &json(&f.state),
                &trusted,
                &json(&f.proof),
                &json(&f.proof.response),
                (NOW + 100_000) as f64
            )
            .is_err()
        );
    }

    #[cfg_attr(not(target_arch = "wasm32"), test)]
    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    fn authority_wraps_only_valid_account_proposals_with_its_own_fresh_receipts() {
        let f = super::super::tests::Fixture::new();
        let domain = PolicyDomain::Account(f.account.clone());
        let original = f.state.head(&domain).unwrap().clone();
        let mut document = f.state.account(&f.account).unwrap().clone();
        document.previous = Some(original.clone());
        document.sequence += 1;
        document.device_enrollment = None;
        document.authenticator_approval = None;
        let proposal = Signed::sign(document, &f.root).unwrap();
        assert!(
            commit_passkey_policy_at_authority(
                &f.state,
                proposal.clone(),
                &key(8),
                NOW + 100_000,
                NOW
            )
            .is_err()
        );
        let committed = commit_passkey_policy_at_authority(
            &f.state,
            proposal.clone(),
            &key(3),
            NOW + 100_000,
            NOW,
        )
        .unwrap();
        assert_eq!(committed.commit.issuer, PublicKey::of(&key(3)));
        assert_eq!(
            committed.commit.payload.dependencies,
            vec![original.clone()]
        );
        assert_eq!(
            committed.commit.payload.expires_at_ms,
            NOW + EXECUTION_LEASE_MS
        );
        assert_eq!(committed.receipts.len(), 1);
        let mut replay = f.state.clone();
        let head = replay.install_account_committed(committed, NOW).unwrap();
        assert_eq!(head.sequence, original.sequence + 1);
        assert_eq!(f.state.head(&domain), Some(&original));
        let mut invalid = proposal;
        invalid.payload.sequence += 1;
        assert!(
            commit_passkey_policy_at_authority(&f.state, invalid, &key(3), NOW + 100_000, NOW)
                .is_err()
        );
        assert_eq!(f.state.head(&domain), Some(&original));
    }
}
