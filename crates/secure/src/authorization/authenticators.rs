//! Opaque account commitments to signed WebAuthn credential metadata.

use super::*;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PasskeyClientEnrollment {
    AccountClient,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct AuthenticatorRecord {
    pub version: u16,
    pub account: AccountId,
    pub credential_id: String,
    pub cose_public_key: Vec<u8>,
    pub rp_id: String,
    pub origins: Vec<String>,
    pub user_handle: String,
    pub backup_eligible: bool,
    pub initial_state: crate::webauthn::AuthenticatorState,
    pub client_enrollment: PasskeyClientEnrollment,
    pub issued_under: PolicyStamp,
    pub not_before_ms: u64,
    pub expires_at_ms: u64,
    pub nonce: Nonce,
    pub enrollment_hash: ObjectHash,
}
impl sealed::Sealed for AuthenticatorRecord {}
impl SignedObject for AuthenticatorRecord {
    const DOMAIN: &'static str = "authenticator-record-v2";
}

#[derive(Clone, Debug)]
pub struct VerifiedAuthenticator {
    record: Signed<AuthenticatorRecord>,
    hash: ObjectHash,
    state_floor: crate::webauthn::AuthenticatorState,
}
impl VerifiedAuthenticator {
    pub fn record(&self) -> &Signed<AuthenticatorRecord> {
        &self.record
    }
    pub fn record_hash(&self) -> &ObjectHash {
        &self.hash
    }
    pub fn account(&self) -> &AccountId {
        &self.record.payload.account
    }
    pub fn credential_id(&self) -> &str {
        &self.record.payload.credential_id
    }
    pub fn cose_public_key(&self) -> &[u8] {
        &self.record.payload.cose_public_key
    }
    pub fn rp_id(&self) -> &str {
        &self.record.payload.rp_id
    }
    pub fn origin(&self) -> &str {
        &self.record.payload.origins[0]
    }
    pub fn user_handle(&self) -> &str {
        &self.record.payload.user_handle
    }
    pub fn backup_eligible(&self) -> bool {
        self.record.payload.backup_eligible
    }
    pub fn state_floor(&self) -> crate::webauthn::AuthenticatorState {
        self.state_floor
    }
    pub(super) fn assertion_state(
        &self,
        claimed: crate::webauthn::AuthenticatorState,
    ) -> Result<crate::webauthn::AuthenticatorState> {
        require(
            claimed.sign_count >= self.state_floor.sign_count
                && claimed.backup_state == self.state_floor.backup_state,
            "passkey state is older than verified policy history",
        )?;
        Ok(claimed)
    }
    pub fn expires_at_ms(&self) -> u64 {
        self.record.payload.expires_at_ms
    }
}

fn base64url(value: &str, max_bytes: usize) -> bool {
    if value.is_empty()
        || value.len() > max_bytes.saturating_mul(4).div_ceil(3)
        || value.len() % 4 == 1
    {
        return false;
    }
    let mut last = 0;
    for byte in value.bytes() {
        last = match byte {
            b'A'..=b'Z' => byte - b'A',
            b'a'..=b'z' => byte - b'a' + 26,
            b'0'..=b'9' => byte - b'0' + 52,
            b'-' => 62,
            b'_' => 63,
            _ => return false,
        };
    }
    match value.len() % 4 {
        2 => last & 15 == 0,
        3 => last & 3 == 0,
        _ => true,
    }
}

impl VerifiedPolicySet {
    pub fn authenticator(&self, hash: &ObjectHash) -> Option<&Signed<AuthenticatorRecord>> {
        self.authenticators.get(hash)
    }
    pub(super) fn validate_authenticator(
        &self,
        signed: &Signed<AuthenticatorRecord>,
        now_ms: u64,
    ) -> Result<()> {
        let record = &signed.payload;
        require(
            record.version == AUTH_VERSION
                && record.issued_under.domain == PolicyDomain::Account(record.account.clone())
                && base64url(&record.credential_id, 1024)
                && !record.cose_public_key.is_empty()
                && record.cose_public_key.len() <= 256
                && !record.rp_id.is_empty()
                && record.rp_id.len() <= 253
                && record.origins.len() == 1
                && !record.origins[0].is_empty()
                && record.origins[0].len() <= 2048
                && record.user_handle.len() == 43
                && base64url(&record.user_handle, 32)
                && (!record.initial_state.backup_state || record.backup_eligible),
            "invalid authenticator record",
        )?;
        validate_window(record.not_before_ms, record.expires_at_ms, now_ms)?;
        let root = self.historical_root(&record.issued_under)?;
        if signed.issuer != root {
            let hash = signed.hash()?;
            let registered = self
                .account(&record.account)
                .is_some_and(|a| a.authenticators.contains(&hash));
            if !registered {
                let expires = self.validate_authenticator_manager(
                    &record.account,
                    &signed.issuer,
                    now_ms,
                    &mut Budget::default(),
                )?;
                require(
                    record.expires_at_ms <= expires,
                    "authenticator exceeds manager authority expiry",
                )?;
            }
        }
        signed.verify_signature()
    }
    pub fn install_authenticator(
        &mut self,
        signed: Signed<AuthenticatorRecord>,
        now_ms: u64,
    ) -> Result<ObjectHash> {
        require(self.authenticators.len() < 4096, "too many authenticators")?;
        self.validate_authenticator(&signed, now_ms)?;
        let hash = signed.hash()?;
        if let Some(account) = self.account(&signed.payload.account)
            && account.authenticators.contains(&hash)
        {
            for other_hash in &account.authenticators {
                if let Some(other) = self.authenticators.get(other_hash) {
                    require(
                        other_hash == &hash
                            || other.payload.credential_id != signed.payload.credential_id,
                        "duplicate registered authenticator",
                    )?;
                }
            }
        }
        self.authenticators.insert(hash.clone(), signed);
        Ok(hash)
    }
    pub fn verify_authenticator(
        &self,
        signed: &Signed<AuthenticatorRecord>,
        now_ms: u64,
    ) -> Result<VerifiedAuthenticator> {
        self.validate_authenticator(signed, now_ms)?;
        let hash = signed.hash()?;
        let account = self
            .account(&signed.payload.account)
            .ok_or(AuthorizationError::Revoked)?;
        if !account.authenticators.contains(&hash) {
            return Err(AuthorizationError::Revoked);
        }
        Ok(VerifiedAuthenticator {
            record: signed.clone(),
            state_floor: self
                .authenticator_states
                .get(&hash)
                .copied()
                .unwrap_or(signed.payload.initial_state),
            hash,
        })
    }
    pub(super) fn validate_authenticator_registration(
        &self,
        document: &AccountDocument,
    ) -> Result<()> {
        let mut credentials = BTreeSet::new();
        for hash in &document.authenticators {
            if let Some(record) = self.authenticators.get(hash) {
                require(
                    record.payload.account == document.account
                        && credentials.insert(&record.payload.credential_id),
                    "invalid registered authenticator",
                )?;
            }
        }
        if let Some(state) = self.accounts.get(&document.account) {
            let previous = &state
                .history
                .last()
                .ok_or(AuthorizationError::PolicyConflict)?
                .payload;
            for old in &state.history {
                for hash in &document.authenticators {
                    require(
                        !old.payload.authenticators.contains(hash)
                            || previous.authenticators.contains(hash),
                        "revoked authenticator cannot be reactivated",
                    )?;
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::{Fixture, NOW};
    use super::*;

    #[test]
    fn authenticator_requires_root_signature_active_commitment_and_current_validity() {
        let mut f = Fixture::new();
        let record = AuthenticatorRecord {
            version: AUTH_VERSION,
            account: f.account.clone(),
            credential_id: "AQID".into(),
            cose_public_key: vec![1, 2, 3],
            rp_id: "example.com".into(),
            origins: vec!["https://example.com".into()],
            user_handle: "A".repeat(43),
            backup_eligible: true,
            initial_state: crate::webauthn::AuthenticatorState {
                sign_count: 0,
                backup_state: false,
            },
            client_enrollment: PasskeyClientEnrollment::AccountClient,
            issued_under: f
                .state
                .head(&PolicyDomain::Account(f.account.clone()))
                .unwrap()
                .clone(),
            not_before_ms: NOW,
            expires_at_ms: NOW + 1000,
            nonce: Nonce::from_bytes([99; 32]),
            enrollment_hash: ObjectHash::body(&"complete enrollment request").unwrap(),
        };
        let signed = Signed::sign(record.clone(), &f.root).unwrap();
        assert_eq!(
            f.state.verify_authenticator(&signed, NOW).unwrap_err(),
            AuthorizationError::Revoked
        );
        assert!(
            f.state
                .install_authenticator(Signed::sign(record.clone(), &f.node).unwrap(), NOW)
                .is_err()
        );
        let hash = f.state.install_authenticator(signed.clone(), NOW).unwrap();
        let mut document = f.state.account(&f.account).unwrap().clone();
        document.previous = f
            .state
            .head(&PolicyDomain::Account(f.account.clone()))
            .cloned();
        document.sequence += 1;
        document.authenticators = vec![hash.clone()];
        f.state
            .install_account(Signed::sign(document.clone(), &f.root).unwrap())
            .unwrap();
        let verified = f.state.verify_authenticator(&signed, NOW).unwrap();
        assert_eq!(verified.record_hash(), &hash);
        assert_eq!(verified.account(), &f.account);
        assert_eq!(verified.credential_id(), "AQID");
        assert!(verified.backup_eligible());
        assert_eq!(verified.state_floor(), record.initial_state);
        assert_eq!(
            f.state
                .verify_authenticator(&signed, NOW + 1000)
                .unwrap_err(),
            AuthorizationError::Expired
        );
        let mut tampered = signed.clone();
        tampered.payload.initial_state.sign_count = 99;
        assert!(f.state.verify_authenticator(&tampered, NOW).is_err());
        let mut invalid = record.clone();
        invalid.backup_eligible = false;
        invalid.initial_state.backup_state = true;
        assert!(
            f.state
                .install_authenticator(Signed::sign(invalid, &f.root).unwrap(), NOW)
                .is_err()
        );
        let mut wire = serde_json::to_value(&signed).unwrap();
        wire["payload"]
            .as_object_mut()
            .unwrap()
            .remove("initial_state");
        assert!(serde_json::from_value::<Signed<AuthenticatorRecord>>(wire).is_err());
        let mut tampered = signed.clone();
        tampered.payload.backup_eligible = false;
        assert!(f.state.verify_authenticator(&tampered, NOW).is_err());
        document.previous = f
            .state
            .head(&PolicyDomain::Account(f.account.clone()))
            .cloned();
        document.sequence += 1;
        document.authenticators.clear();
        f.state
            .install_account(Signed::sign(document.clone(), &f.root).unwrap())
            .unwrap();
        assert_eq!(
            f.state.verify_authenticator(&signed, NOW).unwrap_err(),
            AuthorizationError::Revoked
        );
        document.previous = f
            .state
            .head(&PolicyDomain::Account(f.account.clone()))
            .cloned();
        document.sequence += 1;
        document.authenticators = vec![hash];
        assert!(
            f.state
                .install_account(Signed::sign(document, &f.root).unwrap())
                .is_err()
        );
    }
    #[test]
    fn base64url_requires_canonical_unpadded_encoding() {
        for valid in ["AA", "AAA", "AAAA", "AQID", "-w"] {
            assert!(base64url(valid, 32));
        }
        for invalid in ["", "A", "AB", "AAB", "AA=", "A+", "A/"] {
            assert!(!base64url(invalid, 32));
        }
        assert!(!base64url(&"A".repeat(44), 32));
    }
}
