//! Account policy changes require both a current manager and an existing Passkey.

use super::*;
use crate::webauthn::{
    PasskeyAssertionProof, PasskeyPurpose, RegisteredPasskey, verify_passkey_assertion,
};

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct AuthenticatorManagementIntent {
    pub version: u16,
    pub account: AccountId,
    pub device_public_key: PublicKey,
    pub target: DeviceId,
    pub proposal_hash: ObjectHash,
    pub expected_heads: Vec<PolicyStamp>,
    pub request_id: Nonce,
    pub issued_at_ms: u64,
    pub expires_at_ms: u64,
}
impl sealed::Sealed for AuthenticatorManagementIntent {}
impl SignedObject for AuthenticatorManagementIntent {
    const DOMAIN: &'static str = "authenticator-management-intent-v2";
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct AuthenticatorPolicyApproval {
    pub intent: Signed<AuthenticatorManagementIntent>,
    pub assertion: PasskeyAssertionProof,
}

#[derive(Clone, Debug)]
pub struct VerifiedAuthenticatorManagement {
    intent: Signed<AuthenticatorManagementIntent>,
    hash: ObjectHash,
}
impl VerifiedAuthenticatorManagement {
    pub fn intent(&self) -> &AuthenticatorManagementIntent {
        &self.intent.payload
    }
    pub fn intent_hash(&self) -> &ObjectHash {
        &self.hash
    }
    pub fn account(&self) -> &AccountId {
        &self.intent.payload.account
    }
    pub fn issuer(&self) -> &PublicKey {
        &self.intent.issuer
    }
    pub fn expected_heads(&self) -> &[PolicyStamp] {
        &self.intent.payload.expected_heads
    }
    pub fn expires_at_ms(&self) -> u64 {
        self.intent.payload.expires_at_ms
    }
}

pub fn authenticator_proposal_hash(document: &AccountDocument) -> Result<ObjectHash> {
    let mut document = document.clone();
    document.authenticator_approval = None;
    document.root_acceptance = None;
    document.recovery_approvals.clear();
    ObjectHash::body(&document)
}
#[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen(js_name = accountAuthenticatorProposalHash))]
pub fn account_authenticator_proposal_hash(document_json: &str) -> crate::Result<String> {
    require(
        document_json.len() <= MAX_PROOF_BYTES,
        "authenticator proposal too large",
    )
    .map_err(|e| e.to_string())?;
    let document: AccountDocument =
        serde_json::from_str(document_json).map_err(|_| "invalid authenticator proposal")?;
    authenticator_proposal_hash(&document)
        .map(|h| h.to_string())
        .map_err(|e| e.to_string())
}

impl VerifiedPolicySet {
    pub fn verify_authenticator_management_intent(
        &self,
        signed: &Signed<AuthenticatorManagementIntent>,
        now_ms: u64,
    ) -> Result<VerifiedAuthenticatorManagement> {
        let intent = &signed.payload;
        let account = self
            .account(&intent.account)
            .ok_or(AuthorizationError::Revoked)?;
        let head = self
            .head(&PolicyDomain::Account(intent.account.clone()))
            .ok_or(AuthorizationError::PolicyConflict)?;
        require(
            intent.version == AUTH_VERSION
                && intent.device_public_key == signed.issuer
                && intent.target == account.authority.device
                && intent.expected_heads == [head.clone()],
            "authenticator management intent policy mismatch",
        )?;
        validate_window(intent.issued_at_ms, intent.expires_at_ms, now_ms)?;
        require(
            intent.expires_at_ms - intent.issued_at_ms <= INVOCATION_MAX_AGE_MS,
            "authenticator management intent lifetime too long",
        )?;
        if signed.issuer != account.current_root {
            let deadline = self.validate_authenticator_manager(
                &intent.account,
                &signed.issuer,
                now_ms,
                &mut Budget::default(),
            )?;
            require(
                intent.expires_at_ms <= deadline,
                "authenticator management intent exceeds current manager expiry",
            )?;
        }
        signed.verify_signature()?;
        Ok(VerifiedAuthenticatorManagement {
            intent: signed.clone(),
            hash: ObjectHash::body(intent)?,
        })
    }
    pub(super) fn validate_authenticator_manager(
        &self,
        account: &AccountId,
        key: &PublicKey,
        now: u64,
        budget: &mut Budget,
    ) -> Result<u64> {
        let device = DeviceId::from_key(key);
        let certificate = self.find_device(&device)?;
        require(
            certificate.payload.subject == Actor::Account(account.clone())
                && certificate.payload.public_key == *key,
            "authenticator manager belongs to another account",
        )?;
        self.validate_certificate(
            certificate,
            now,
            budget,
            &mut BTreeMap::new(),
            &mut BTreeSet::new(),
        )?;
        let authority = self
            .account(account)
            .and_then(|a| a.enrollment.iter().find(|e| e.device == device))
            .ok_or(AuthorizationError::Revoked)?;
        require(
            now < authority.expires_at_ms
                && authority
                    .rights
                    .contains(&AccountRight::ManageAuthenticators),
            "account device lacks authenticator management right",
        )?;
        budget.expires(authority.expires_at_ms);
        Ok(authority
            .expires_at_ms
            .min(certificate.payload.expires_at_ms))
    }
    pub(super) fn authorize_authenticator_delta(
        &self,
        signed: &Signed<AccountDocument>,
        previous: &AccountDocument,
        now: u64,
        budget: &mut Budget,
    ) -> Result<()> {
        let document = &signed.payload;
        if document.authenticators == previous.authenticators {
            return require(
                document.authenticator_approval.is_none(),
                "unexpected authenticator approval",
            );
        }
        self.validate_authenticator_manager(&document.account, &signed.issuer, now, budget)?;
        let approval = document
            .authenticator_approval
            .as_ref()
            .ok_or(AuthorizationError::UserPresenceRequired)?;
        let token = self.verify_authenticator_management_intent(&approval.intent, now)?;
        require(
            token.issuer() == &signed.issuer
                && token.account() == &document.account
                && token.intent().proposal_hash == authenticator_proposal_hash(document)?,
            "authenticator approval does not bind policy proposal",
        )?;
        let envelope = approval.assertion.pending.envelope();
        require(
            envelope.purpose == PasskeyPurpose::ManageAuthenticators
                && envelope.account == document.account
                && envelope.device_public_key == signed.issuer
                && envelope.rights == [AccountRight::ManageAuthenticators]
                && envelope.target == token.intent().target
                && envelope.expected_heads == token.intent().expected_heads
                && envelope.enrollment_hash == *token.intent_hash()
                && envelope.not_before_ms >= token.intent().issued_at_ms
                && envelope.expires_at_ms <= token.expires_at_ms(),
            "passkey management envelope mismatch",
        )?;
        budget.verify(&approval.intent)?;
        budget.verify(&approval.assertion.record)?;
        budget.external_signature(ObjectHash::body(&approval.assertion)?)?;
        budget.bytes = budget
            .bytes
            .checked_add(canonical(&approval.assertion)?.len())
            .ok_or(AuthorizationError::BudgetExceeded)?;
        if budget.bytes > MAX_PROOF_BYTES {
            return Err(AuthorizationError::BudgetExceeded);
        }
        let old = self.verify_authenticator(&approval.assertion.record, now)?;
        require(
            old.account() == &document.account,
            "passkey approval belongs to another account",
        )?;
        let credential = RegisteredPasskey::from_authorized(&old, approval.assertion.state)
            .map_err(|_| {
                AuthorizationError::InvalidProof("invalid registered management passkey")
            })?;
        verify_passkey_assertion(
            &approval.assertion.pending,
            &credential,
            &approval.assertion.response,
            now,
        )
        .map_err(|_| AuthorizationError::InvalidProof("invalid management passkey approval"))?;
        budget.expires(
            token
                .expires_at_ms()
                .min(envelope.expires_at_ms)
                .min(old.expires_at_ms()),
        );
        let head = self
            .head(&PolicyDomain::Account(document.account.clone()))
            .ok_or(AuthorizationError::PolicyConflict)?;
        budget
            .dependencies
            .insert(head.domain.clone(), head.clone());
        for hash in document
            .authenticators
            .iter()
            .filter(|h| !previous.authenticators.contains(h))
        {
            let record = self
                .authenticator(hash)
                .ok_or(AuthorizationError::InvalidProof(
                    "missing new authenticator record",
                ))?;
            require(
                record.issuer == signed.issuer && record.payload.account == document.account,
                "authenticator registration issuer mismatch",
            )?;
            self.validate_authenticator(record, now)?;
            budget.verify(record)?;
        }
        Ok(())
    }
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen(js_name = buildAuthenticatorManagementIntent))]
pub fn build_authenticator_management_intent(
    auth: &VerifiedClientAuthorization,
    bundle_json: &str,
    pins_json: &str,
    proposal_json: &str,
    identity: &Identity,
    now_ms: f64,
    expires_at_ms: f64,
) -> crate::Result<String> {
    let build = || -> Result<Signed<AuthenticatorManagementIntent>> {
        require(
            bundle_json.len() <= 1_048_576
                && pins_json.len() <= MAX_PROOF_BYTES
                && proposal_json.len() <= MAX_PROOF_BYTES,
            "authenticator management input too large",
        )?;
        require(
            auth.scope.device_id == DeviceId::from_key(&PublicKey::of(identity)),
            "authenticator manager key mismatch",
        )?;
        let bundle: ClientAuthorizationBundle = serde_json::from_str(bundle_json)
            .map_err(|_| AuthorizationError::InvalidProof("invalid management journal"))?;
        let pins: Vec<PolicyPin> = serde_json::from_str(pins_json)
            .map_err(|_| AuthorizationError::InvalidProof("invalid management pins"))?;
        let state = replay_policies(&pins, &bundle.journal)?;
        for head in auth.heads() {
            require(
                state.head(&head.domain) == Some(head),
                "management authorization policy changed",
            )?;
        }
        let proposal: AccountDocument = serde_json::from_str(proposal_json)
            .map_err(|_| AuthorizationError::InvalidProof("invalid authenticator proposal"))?;
        require(
            proposal.account == auth.scope.account_id,
            "authenticator proposal account mismatch",
        )?;
        let signed = create_management_intent(
            &state,
            &proposal,
            identity,
            bootstrap::timestamp(now_ms)?,
            bootstrap::timestamp(expires_at_ms)?,
        )?;
        state.verify_authenticator_management_intent(&signed, bootstrap::timestamp(now_ms)?)?;
        Ok(signed)
    };
    serde_json::to_string(&build().map_err(|e| e.to_string())?)
        .map_err(|_| "management intent encoding failed".into())
}

pub(super) fn create_management_intent(
    state: &VerifiedPolicySet,
    proposal: &AccountDocument,
    identity: &Identity,
    now: u64,
    expires: u64,
) -> Result<Signed<AuthenticatorManagementIntent>> {
    let current = state
        .account(&proposal.account)
        .ok_or(AuthorizationError::Revoked)?;
    let head = state
        .head(&PolicyDomain::Account(proposal.account.clone()))
        .ok_or(AuthorizationError::PolicyConflict)?
        .clone();
    require(
        proposal.previous.as_ref() == Some(&head)
            && proposal.authenticators != current.authenticators,
        "authenticator management requires a new current-head proposal",
    )?;
    let signed = Signed::sign(
        AuthenticatorManagementIntent {
            version: AUTH_VERSION,
            account: proposal.account.clone(),
            device_public_key: PublicKey::of(identity),
            target: current.authority.device.clone(),
            proposal_hash: authenticator_proposal_hash(proposal)?,
            expected_heads: vec![head],
            request_id: Nonce::random()?,
            issued_at_ms: now,
            expires_at_ms: expires,
        },
        identity,
    )?;
    state.verify_authenticator_management_intent(&signed, now)?;
    Ok(signed)
}

#[cfg(test)]
mod tests {
    use super::super::tests::{Fixture, NOW};
    use super::*;
    use crate::webauthn::{
        AssertionResponse, AuthenticationCeremony, AuthenticatorAssertion, AuthenticatorState,
        EnrollmentEnvelope, PendingPasskey, RelyingParty,
    };
    use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
    use coset::{CborSerializable, CoseKeyBuilder, iana};
    use p256::ecdsa::{SigningKey, signature::Signer};

    fn proposal(add: bool) -> (Fixture, Signed<AccountDocument>) {
        let mut f = Fixture::new();
        let manager = Identity::from_secret_hex(&"04".repeat(32)).unwrap();
        let signer = SigningKey::from_bytes((&[42; 32]).into()).unwrap();
        let point = signer.verifying_key().to_encoded_point(false);
        let cose = CoseKeyBuilder::new_ec2_pub_key(
            iana::EllipticCurve::P_256,
            point.x().unwrap().to_vec(),
            point.y().unwrap().to_vec(),
        )
        .algorithm(iana::Algorithm::ES256)
        .build()
        .to_vec()
        .unwrap();
        let mut current = f.state.account(&f.account).unwrap().clone();
        let head = f
            .state
            .head(&PolicyDomain::Account(f.account.clone()))
            .unwrap()
            .clone();
        let record = Signed::sign(
            AuthenticatorRecord {
                version: AUTH_VERSION,
                account: f.account.clone(),
                credential_id: URL_SAFE_NO_PAD.encode([9; 32]),
                cose_public_key: cose,
                rp_id: "client.example".into(),
                origins: vec!["https://client.example".into()],
                user_handle: URL_SAFE_NO_PAD.encode([7; 32]),
                backup_eligible: true,
                client_enrollment: PasskeyClientEnrollment::AccountClient,
                issued_under: head.clone(),
                not_before_ms: 0,
                expires_at_ms: 1_000_000,
                nonce: Nonce::from_bytes([40; 32]),
                enrollment_hash: ObjectHash::body(&"real registration context").unwrap(),
            },
            &f.root,
        )
        .unwrap();
        let hash = f.state.install_authenticator(record.clone(), NOW).unwrap();
        current.previous = Some(head);
        current.sequence += 1;
        current.authenticators = vec![hash];
        current.enrollment = vec![EnrollmentAuthority {
            device: DeviceId::from_key(&PublicKey::of(&manager)),
            rights: vec![AccountRight::ManageAuthenticators],
            expires_at_ms: 1_000_000,
        }];
        f.state
            .install_account(Signed::sign(current.clone(), &f.root).unwrap())
            .unwrap();
        current.previous = f
            .state
            .head(&PolicyDomain::Account(f.account.clone()))
            .cloned();
        current.sequence += 1;
        if add {
            let mut payload = record.payload.clone();
            payload.credential_id = URL_SAFE_NO_PAD.encode([10; 32]);
            payload.issued_under = current.previous.clone().unwrap();
            payload.nonce = Nonce::from_bytes([44; 32]);
            let new_record = Signed::sign(payload, &manager).unwrap();
            current
                .authenticators
                .push(f.state.install_authenticator(new_record, NOW).unwrap());
            current.authenticators.sort();
        } else {
            current.authenticators.clear();
        }
        let intent =
            create_management_intent(&f.state, &current, &manager, NOW, NOW + 10_000).unwrap();
        let pending = PendingPasskey::prepare(
            EnrollmentEnvelope {
                version: AUTH_VERSION,
                purpose: PasskeyPurpose::ManageAuthenticators,
                account: f.account.clone(),
                device_public_key: PublicKey::of(&manager),
                rights: vec![AccountRight::ManageAuthenticators],
                target: intent.payload.target.clone(),
                enrollment_hash: ObjectHash::body(&intent.payload).unwrap(),
                expected_heads: intent.payload.expected_heads.clone(),
                nonce: Nonce::from_bytes([41; 32]),
                not_before_ms: NOW,
                expires_at_ms: NOW + 10_000,
            },
            RelyingParty::new("client.example", "https://client.example").unwrap(),
            &[7; 32],
            AuthenticationCeremony::IdentifiedAccount,
            vec![record.payload.credential_id.clone()],
            NOW,
        )
        .unwrap();
        let challenge = pending.authentication_options().unwrap()["publicKey"]["challenge"]
            .as_str()
            .unwrap()
            .to_owned();
        let client=serde_json::to_vec(&serde_json::json!({"type":"webauthn.get","challenge":challenge,"origin":"https://client.example","crossOrigin":false})).unwrap();
        let mut data = Sha256::digest(b"client.example").to_vec();
        data.push(0x0d);
        data.extend(1u32.to_be_bytes());
        let mut signed = data.clone();
        signed.extend(Sha256::digest(&client));
        let signature: p256::ecdsa::Signature = signer.sign(&signed);
        let response = AssertionResponse {
            id: record.payload.credential_id.clone(),
            raw_id: record.payload.credential_id.clone(),
            credential_type: "public-key".into(),
            response: AuthenticatorAssertion {
                client_data_json: URL_SAFE_NO_PAD.encode(client),
                authenticator_data: URL_SAFE_NO_PAD.encode(data),
                signature: URL_SAFE_NO_PAD.encode(signature.to_der()),
                user_handle: Some(record.payload.user_handle.clone()),
            },
            client_extension_results: serde_json::json!({}),
        };
        current.authenticator_approval = Some(AuthenticatorPolicyApproval {
            intent,
            assertion: PasskeyAssertionProof {
                pending,
                record,
                state: AuthenticatorState {
                    sign_count: 0,
                    backup_state: false,
                },
                response,
            },
        });
        (f, Signed::sign(current, &manager).unwrap())
    }
    fn committed(f: &Fixture, proposal: Signed<AccountDocument>) -> Committed<AccountDocument> {
        let previous = f
            .state
            .head(&PolicyDomain::Account(f.account.clone()))
            .unwrap()
            .clone();
        let authority = Identity::from_secret_hex(&"03".repeat(32)).unwrap();
        let commit = Signed::sign(
            PolicyCommit {
                version: AUTH_VERSION,
                previous: previous.clone(),
                proposal_hash: proposal.hash().unwrap(),
                accepted_at_ms: NOW,
                issued_at_ms: NOW,
                expires_at_ms: NOW + 10_000,
                nonce: Nonce::from_bytes([42; 32]),
                dependencies: vec![previous.clone()],
            },
            &authority,
        )
        .unwrap();
        let receipt = Signed::sign(
            FreshnessReceipt {
                version: AUTH_VERSION,
                current: previous,
                challenge_hash: commit.challenge_hash().unwrap(),
                issued_at_ms: NOW,
                expires_at_ms: NOW + 10_000,
            },
            &authority,
        )
        .unwrap();
        Committed {
            proposal,
            commit,
            receipts: vec![receipt],
        }
    }
    #[test]
    fn management_commits_require_the_current_manager_and_real_bound_uv_assertion() {
        let (mut f, signed) = proposal(false);
        let manager = Identity::from_secret_hex(&"04".repeat(32)).unwrap();
        for field in 0..4 {
            let mut changed = signed.payload.clone();
            match field {
                0 => changed.authenticator_approval = None,
                1 => {
                    changed
                        .authenticator_approval
                        .as_mut()
                        .unwrap()
                        .assertion
                        .response
                        .response
                        .signature = URL_SAFE_NO_PAD.encode([0; 64])
                }
                2 => {
                    changed
                        .authenticator_approval
                        .as_mut()
                        .unwrap()
                        .intent
                        .payload
                        .proposal_hash = ObjectHash::body(&"another operation").unwrap()
                }
                _ => {
                    changed.current_root =
                        PublicKey::of(&Identity::from_secret_hex(&"45".repeat(32)).unwrap())
                }
            }
            let proposal = Signed::sign(changed, &manager).unwrap();
            assert!(
                f.state
                    .install_account_committed(committed(&f, proposal), NOW)
                    .is_err()
            );
        }
        let receipt = committed(&f, signed);
        let old = receipt
            .proposal
            .payload
            .authenticator_approval
            .as_ref()
            .unwrap()
            .assertion
            .record
            .clone();
        f.state.install_account_committed(receipt, NOW).unwrap();
        assert!(
            f.state
                .account(&f.account)
                .unwrap()
                .authenticators
                .is_empty()
        );
        assert_eq!(
            f.state.verify_authenticator(&old, NOW).unwrap_err(),
            AuthorizationError::Revoked
        );
    }
    #[test]
    fn manager_signed_credentials_require_uv_and_survive_the_original_managers_revocation() {
        let (mut f, signed) = proposal(true);
        let previous = f.state.account(&f.account).unwrap().authenticators.clone();
        let added = signed
            .payload
            .authenticators
            .iter()
            .find(|h| !previous.contains(h))
            .unwrap()
            .clone();
        let record = f.state.authenticator(&added).unwrap().clone();
        assert_eq!(
            f.state.verify_authenticator(&record, NOW).unwrap_err(),
            AuthorizationError::Revoked
        );
        f.state
            .install_account_committed(committed(&f, signed), NOW)
            .unwrap();
        assert!(f.state.verify_authenticator(&record, NOW).is_ok());
        let mut current = f.state.account(&f.account).unwrap().clone();
        current.previous = f
            .state
            .head(&PolicyDomain::Account(f.account.clone()))
            .cloned();
        current.sequence += 1;
        current.authenticator_approval = None;
        let manager = DeviceId::from_key(&PublicKey::of(
            &Identity::from_secret_hex(&"04".repeat(32)).unwrap(),
        ));
        current
            .devices
            .retain(|h| f.state.device(h).unwrap().payload.device != manager);
        current.revoked_devices.push(manager);
        current.revoked_devices.sort();
        current.enrollment.clear();
        f.state
            .install_account(Signed::sign(current, &f.root).unwrap())
            .unwrap();
        assert!(f.state.verify_authenticator(&record, NOW).is_ok());
    }

    #[test]
    fn explicit_management_retries_have_distinct_intent_identity_and_share_proposal_hash() {
        let (f, signed) = proposal(false);
        let manager = Identity::from_secret_hex(&"04".repeat(32)).unwrap();
        let first =
            create_management_intent(&f.state, &signed.payload, &manager, NOW, NOW + 10_000)
                .unwrap();
        let second =
            create_management_intent(&f.state, &signed.payload, &manager, NOW, NOW + 10_000)
                .unwrap();
        assert_eq!(first.payload.proposal_hash, second.payload.proposal_hash);
        assert_ne!(
            ObjectHash::body(&first.payload).unwrap(),
            ObjectHash::body(&second.payload).unwrap()
        );
        let mut state = f.state.clone();
        let mut account = state.account(&f.account).unwrap().clone();
        account.previous = state
            .head(&PolicyDomain::Account(f.account.clone()))
            .cloned();
        account.sequence += 1;
        account.enrollment[0].expires_at_ms = NOW + 100;
        state
            .install_account(Signed::sign(account, &f.root).unwrap())
            .unwrap();
        let mut next = state.account(&f.account).unwrap().clone();
        next.previous = state
            .head(&PolicyDomain::Account(f.account.clone()))
            .cloned();
        next.sequence += 1;
        next.authenticators.clear();
        assert!(create_management_intent(&state, &next, &manager, NOW, NOW + 10_000).is_err());
        assert!(create_management_intent(&state, &next, &manager, NOW, NOW + 100).is_ok());
    }
}
