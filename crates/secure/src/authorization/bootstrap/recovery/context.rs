//! Recovery credentials authorize a bounded read before a new device can use Noise.

use super::*;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RecoveryContextRequest {
    pub version: u16,
    pub account: AccountId,
    pub space: SpaceId,
    pub new_device_public_key: PublicKey,
    pub node_public_key: PublicKey,
    pub nonce: Nonce,
    pub issued_at_ms: u64,
    pub expires_at_ms: u64,
}
impl sealed::Sealed for RecoveryContextRequest {}
impl SignedObject for RecoveryContextRequest {
    const DOMAIN: &'static str = "recovery-context-request-v2";
}

#[derive(Clone, Debug)]
#[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen)]
pub struct VerifiedRecoveryContext {
    request: Signed<RecoveryContextRequest>,
    hash: ObjectHash,
}
impl VerifiedRecoveryContext {
    pub fn account(&self) -> &AccountId {
        &self.request.payload.account
    }
    pub fn space(&self) -> &SpaceId {
        &self.request.payload.space
    }
    pub fn new_device_public_key(&self) -> &PublicKey {
        &self.request.payload.new_device_public_key
    }
    pub fn node_public_key(&self) -> &PublicKey {
        &self.request.payload.node_public_key
    }
    pub fn nonce(&self) -> &Nonce {
        &self.request.payload.nonce
    }
    pub fn expires_at_ms(&self) -> u64 {
        self.request.payload.expires_at_ms
    }
    pub fn request_hash(&self) -> &ObjectHash {
        &self.hash
    }
}
#[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen)]
impl VerifiedRecoveryContext {
    #[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen(js_name = contextJson))]
    pub fn context_json(&self) -> crate::Result<String> {
        encode(
            &serde_json::json!({"account":self.account(),"space":self.space(),
            "new_device_public_key":self.new_device_public_key(),"node_public_key":self.node_public_key(),
            "nonce":self.nonce(),"expires_at_ms":self.expires_at_ms(),"request_hash":self.request_hash()}),
        )
    }
}

impl VerifiedPolicySet {
    pub fn verify_recovery_context_request(
        &self,
        signed: &Signed<RecoveryContextRequest>,
        local_node: &PublicKey,
        now_ms: u64,
    ) -> Result<VerifiedRecoveryContext> {
        let request = &signed.payload;
        let account = self
            .account(&request.account)
            .ok_or(AuthorizationError::Revoked)?;
        let space = self
            .space(&request.space)
            .ok_or(AuthorizationError::Revoked)?;
        require(
            request.version == AUTH_VERSION
                && request.node_public_key == *local_node
                && request.space == SpaceId::personal(&request.account)
                && space.root
                    == SpaceRoot::Personal {
                        owner: request.account.clone(),
                        genesis_root: account.genesis_root.clone(),
                    }
                && account.recovery.threshold == 1
                && account.recovery.keys == [signed.issuer.clone()]
                && request.new_device_public_key != signed.issuer
                && request.new_device_public_key != request.node_public_key
                && request.new_device_public_key != account.current_root
                && !self
                    .devices
                    .values()
                    .any(|c| c.payload.public_key == request.new_device_public_key),
            "recovery context is not authorized for this account and node",
        )?;
        validate_window(request.issued_at_ms, request.expires_at_ms, now_ms)?;
        require(
            request.expires_at_ms - request.issued_at_ms <= NODE_CLAIM_MAX_AGE_MS,
            "recovery context validity too long",
        )?;
        signed.verify_signature()?;
        Ok(VerifiedRecoveryContext {
            request: signed.clone(),
            hash: signed.hash()?,
        })
    }
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen(js_name = buildRecoveryContextRequest))]
pub fn build_recovery_context_request(
    material_json: &str,
    new_client: &Identity,
    expected_node_public_key: &str,
    now_ms: f64,
    expires_at_ms: f64,
) -> crate::Result<String> {
    let build = || -> Result<Signed<RecoveryContextRequest>> {
        let material = read_material(material_json)?;
        let recovery = validate_material(&material)?;
        let node = PublicKey::parse(expected_node_public_key)?;
        require(
            node == material.entry.node_public_key,
            "recovery context node pin mismatch",
        )?;
        let request = Signed::sign(
            RecoveryContextRequest {
                version: AUTH_VERSION,
                account: material.scope.account_id.clone(),
                space: material.scope.space_id.clone(),
                new_device_public_key: PublicKey::of(new_client),
                node_public_key: node.clone(),
                nonce: Nonce::random()?,
                issued_at_ms: timestamp(now_ms)?,
                expires_at_ms: timestamp(expires_at_ms)?,
            },
            &recovery,
        )?;
        replay_policies(&material.pins, &material.bundle.journal)?
            .verify_recovery_context_request(&request, &node, timestamp(now_ms)?)?;
        Ok(request)
    };
    encode(&build().map_err(|e| e.to_string())?)
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen(js_name = verifyRecoveryContextRequest))]
pub fn verify_recovery_context_request_json(
    request_json: &str,
    trusted_pins_json: &str,
    trusted_bundle_json: &str,
    local_node_public_key: &str,
    now_ms: f64,
) -> crate::Result<VerifiedRecoveryContext> {
    let verify = || -> Result<VerifiedRecoveryContext> {
        require(
            trusted_pins_json.len() <= MAX_PROOF_BYTES,
            "recovery context pins too large",
        )?;
        let pins: Vec<PolicyPin> = serde_json::from_str(trusted_pins_json)
            .map_err(|_| AuthorizationError::InvalidProof("invalid recovery context pins"))?;
        let bundle = read_bundle(trusted_bundle_json)?;
        let request = decode_signed(request_json.as_bytes())?;
        replay_policies(&pins, &bundle.journal)?.verify_recovery_context_request(
            &request,
            &PublicKey::parse(local_node_public_key)?,
            timestamp(now_ms)?,
        )
    };
    verify().map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn context_read_requires_current_recovery_key_node_pin_and_new_client() {
        let key = |n| Identity::from_secret_hex(&hex::encode([n; 32])).unwrap();
        let nonce = Nonce::from_bytes([8; 32]);
        let claim = verify_node_claim(
            &create_node_claim(&key(4), nonce.as_str(), 1000.0, 100_000.0).unwrap(),
            &key(4).public_key_hex(),
            nonce.as_str(),
            1000.0,
        )
        .unwrap();
        let prepared =
            prepare_account_creation(&key(1), &key(2), &key(3), &claim, 1_000_000.0, 1000.0)
                .unwrap();
        let proof = sign_node_possession(
            &prepared.node_request_json().unwrap(),
            &key(4),
            nonce.as_str(),
            1000.0,
        )
        .unwrap();
        let created = finish_account_creation(&prepared, &key(1), &proof, 1000.0).unwrap();
        let material = build_account_recovery_material(
            &created,
            &key(1),
            &key(2),
            "https://node.example",
            1000.0,
        )
        .unwrap();
        let request = build_recovery_context_request(
            &material,
            &key(5),
            &key(4).public_key_hex(),
            2000.0,
            100_000.0,
        )
        .unwrap();
        let verify = |request: &str, node: &Identity, now| {
            verify_recovery_context_request_json(
                request,
                &created.pins_json().unwrap(),
                &created.bundle_json().unwrap(),
                &node.public_key_hex(),
                now,
            )
        };
        let verified = verify(&request, &key(4), 2000.0).unwrap();
        assert_eq!(verified.new_device_public_key(), &PublicKey::of(&key(5)));
        assert!(verify(&request, &key(6), 2000.0).is_err());
        assert!(verify(&request, &key(4), 100_000.0).is_err());
        assert!(
            build_recovery_context_request(
                &material,
                &key(3),
                &key(4).public_key_hex(),
                2000.0,
                100_000.0
            )
            .is_err()
        );
        assert!(
            build_recovery_context_request(
                &material,
                &key(5),
                &key(6).public_key_hex(),
                2000.0,
                100_000.0
            )
            .is_err()
        );
        let mut signed: Signed<RecoveryContextRequest> = serde_json::from_str(&request).unwrap();
        signed.payload.space = SpaceId::team(&PublicKey::of(&key(6)));
        assert!(
            verify(
                &encode(&Signed::sign(signed.payload, &key(2)).unwrap()).unwrap(),
                &key(4),
                2000.0
            )
            .is_err()
        );
        let mut signed: Signed<RecoveryContextRequest> = serde_json::from_str(&request).unwrap();
        signed.issuer = PublicKey::of(&key(6));
        assert!(verify(&encode(&signed).unwrap(), &key(4), 2000.0).is_err());
    }
}
