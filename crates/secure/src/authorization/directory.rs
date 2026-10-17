//! Complete account device inventory for an already authorized device manager.

use super::*;

#[derive(Serialize)]
struct DirectoryEntry {
    certificate: Signed<DeviceCertificate>,
    status: &'static str,
}
#[derive(Serialize)]
struct DeviceDirectory {
    account: AccountId,
    head: PolicyStamp,
    devices: Vec<DirectoryEntry>,
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen(js_name = verifyAccountDeviceDirectory))]
pub fn verify_account_device_directory(
    auth: &VerifiedClientAuthorization,
    bundle_json: &str,
    pins_json: &str,
    devices_json: &str,
    now_ms: f64,
) -> crate::Result<String> {
    let verify = || -> Result<DeviceDirectory> {
        require(
            bundle_json.len() <= 1_048_576
                && pins_json.len() <= MAX_PROOF_BYTES
                && devices_json.len() <= 2_097_152,
            "account directory too large",
        )?;
        let now = bootstrap::timestamp(now_ms)?;
        let bundle: ClientAuthorizationBundle = serde_json::from_str(bundle_json)
            .map_err(|_| AuthorizationError::InvalidProof("invalid directory policy bundle"))?;
        let pins: Vec<PolicyPin> = serde_json::from_str(pins_json)
            .map_err(|_| AuthorizationError::InvalidProof("invalid directory policy pins"))?;
        let mut state = replay_policies(&pins, &bundle.journal)?;
        for stamp in auth.heads() {
            require(
                state.head(&stamp.domain) == Some(stamp),
                "directory authorization policy changed",
            )?;
        }
        let client = state.find_device(&auth.scope.device_id)?;
        let checked = verify_client_authorization(
            bundle_json,
            pins_json,
            auth.scope.account_id.as_str(),
            auth.scope.space_id.as_str(),
            client.payload.public_key.as_str(),
            now_ms,
        )
        .map_err(|_| AuthorizationError::InvalidProof("invalid current directory authorization"))?;
        require(
            checked.grant_leaf == auth.grant_leaf
                && checked.scope == auth.scope
                && checked
                    .account_enrollment_rights
                    .iter()
                    .any(|r| matches!(r, AccountRight::EnrollDevice | AccountRight::RevokeDevice)),
            "account device management right required",
        )?;
        let account = state
            .account(&auth.scope.account_id)
            .ok_or(AuthorizationError::Revoked)?
            .clone();
        let head = state
            .head(&PolicyDomain::Account(account.account.clone()))
            .ok_or(AuthorizationError::PolicyConflict)?
            .clone();
        let supplied: Vec<Signed<DeviceCertificate>> = serde_json::from_str(devices_json)
            .map_err(|_| AuthorizationError::InvalidProof("invalid directory device list"))?;
        require(supplied.len() <= 1024, "too many directory devices")?;
        let mut seen = BTreeSet::new();
        for certificate in supplied {
            let hash = certificate.hash()?;
            require(
                account.devices.contains(&hash)
                    && seen.insert(hash.clone())
                    && certificate.payload.owner == PolicyDomain::Account(account.account.clone()),
                "directory includes an unregistered or duplicate device",
            )?;
            certificate.verify_signature()?;
            state.devices.insert(hash, certificate);
        }
        let mut devices = Vec::new();
        for hash in &account.devices {
            let certificate = state.device(hash).ok_or(AuthorizationError::InvalidProof(
                "incomplete account device directory",
            ))?;
            // Historical validation displays expired records without granting them a live lease.
            state.validate_certificate(
                certificate,
                certificate.payload.not_before_ms,
                &mut Budget::default(),
                &mut BTreeMap::new(),
                &mut BTreeSet::new(),
            )?;
            require(
                certificate.payload.owner == PolicyDomain::Account(account.account.clone())
                    && !account
                        .revoked_devices
                        .contains(&certificate.payload.device),
                "invalid account directory device",
            )?;
            let status = if now < certificate.payload.not_before_ms {
                "pending"
            } else if now >= certificate.payload.expires_at_ms {
                "expired"
            } else {
                "active"
            };
            devices.push(DirectoryEntry {
                certificate: certificate.clone(),
                status,
            });
        }
        Ok(DeviceDirectory {
            account: account.account,
            head,
            devices,
        })
    };
    serde_json::to_string(&verify().map_err(|e| e.to_string())?)
        .map_err(|_| "directory encoding failed".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn directory_verifies_manager_current_policy_and_each_registered_certificate() {
        use super::super::bootstrap::*;
        let key = |n| Identity::from_secret_hex(&hex::encode([n; 32])).unwrap();
        let nonce = Nonce::from_bytes([9; 32]);
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
        let scope: ClientScope = serde_json::from_str(&created.scope_json().unwrap()).unwrap();
        let bundle = created.bundle_json().unwrap();
        let pins = created.pins_json().unwrap();
        let auth = verify_client_authorization(
            &bundle,
            &pins,
            scope.account_id.as_str(),
            scope.space_id.as_str(),
            &key(3).public_key_hex(),
            1000.0,
        )
        .unwrap();
        let result: serde_json::Value = serde_json::from_str(
            &verify_account_device_directory(&auth, &bundle, &pins, "[]", 1000.0).unwrap(),
        )
        .unwrap();
        assert_eq!(result["devices"].as_array().unwrap().len(), 2);
        assert!(
            result["devices"]
                .as_array()
                .unwrap()
                .iter()
                .all(|d| d["status"] == "active")
        );
        let bundle_value: ClientAuthorizationBundle = serde_json::from_str(&bundle).unwrap();
        let cert = bundle_value
            .journal
            .iter()
            .find_map(|e| {
                if let JournalEntry::Device(c) = e {
                    Some(c.clone())
                } else {
                    None
                }
            })
            .unwrap();
        let duplicate = serde_json::to_string(&vec![cert.clone(), cert.clone()]).unwrap();
        assert!(
            verify_account_device_directory(&auth, &bundle, &pins, &duplicate, 1000.0).is_err()
        );
        let mut forged = cert;
        forged.payload.public_key = PublicKey::of(&key(5));
        assert!(
            verify_account_device_directory(
                &auth,
                &bundle,
                &pins,
                &serde_json::to_string(&vec![forged]).unwrap(),
                1000.0
            )
            .is_err()
        );
        assert!(verify_account_device_directory(&auth, &bundle, &pins, "[]", 1_000_000.0).is_err());
    }
}
