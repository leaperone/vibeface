//! Finite account device inventory for a confirmed device manager.

use super::*;
use crate::signature_verification::{ScopedSignatureVerification, SignatureVerification};
use client_policy_import::{
    record_confirmed_client_policy_metadata, require_confirmed_client_policy,
};
use execution_handoff::ExecutionProofUsage;

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
    next_cursor: Option<ObjectHash>,
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen(js_name = verifyAccountDeviceDirectory))]
pub fn verify_account_device_directory(
    auth: &VerifiedClientAuthorization,
    cursor_json: &str,
    now_ms: f64,
) -> crate::Result<String> {
    let _signature_scope = ScopedSignatureVerification::new();
    let verify = || -> Result<DeviceDirectory> {
        require(
            cursor_json.len() <= MAX_PROOF_BYTES,
            "account directory too large",
        )?;
        let snapshot = require_confirmed_client_policy(auth)?;
        let state = snapshot.state();
        let now = bootstrap::timestamp(now_ms)?;
        let cursor: Option<ObjectHash> = serde_json::from_str(cursor_json)
            .map_err(|_| AuthorizationError::InvalidProof("invalid account directory cursor"))?;
        for stamp in auth.heads() {
            require(
                state.head(&stamp.domain) == Some(stamp),
                "directory authorization policy changed",
            )?;
        }
        let client = state.find_device(&auth.scope.device_id)?;
        let selected = &auth
            .resources
            .first()
            .ok_or(AuthorizationError::InvalidProof(
                "account directory requires a selected client resource",
            ))?
            .payload;
        let checked = verify_client_policy_state_selected(
            state.clone(),
            &ClientAuthorizationBundle {
                journal: Vec::new(),
                grant_leaf: auth.grant_leaf.clone(),
            },
            snapshot.pins(),
            auth.scope.account_id.clone(),
            auth.scope.space_id.clone(),
            client.payload.public_key.clone(),
            Some(&ResourceRef {
                space: selected.space.clone(),
                resource: selected.resource.clone(),
            }),
            now,
        )?;
        require(
            checked.grant_leaf == auth.grant_leaf
                && checked.scope == auth.scope
                && checked.heads == auth.heads
                && checked.audience == auth.audience
                && checked
                    .account_enrollment_rights
                    .iter()
                    .any(|r| matches!(r, AccountRight::EnrollDevice | AccountRight::RevokeDevice)),
            "account device management right required",
        )?;
        let account = state
            .account(&auth.scope.account_id)
            .ok_or(AuthorizationError::Revoked)?;
        let head = state
            .head(&PolicyDomain::Account(account.account.clone()))
            .ok_or(AuthorizationError::PolicyConflict)?
            .clone();
        let index = match cursor.as_ref() {
            Some(cursor) => {
                account.devices.binary_search(cursor).map_err(|_| {
                    AuthorizationError::InvalidProof("account directory cursor is not registered")
                })? + 1
            }
            None => 0,
        };
        let candidate = account.devices.get(index);
        let next_cursor = if index.saturating_add(1) < account.devices.len() {
            candidate.cloned()
        } else {
            None
        };
        let mut budget = checked.budget;
        let mut devices = Vec::new();
        if let Some(hash) = candidate {
            let certificate = state.device(hash).ok_or(AuthorizationError::InvalidProof(
                "incomplete account device directory",
            ))?;
            // Historical validation displays expired records without granting them a live lease.
            state.validate_certificate(
                certificate,
                certificate.payload.not_before_ms,
                &mut budget,
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
        let mut usage = ExecutionProofUsage::default();
        record_confirmed_client_policy_metadata(auth, &mut usage)?;
        usage.record_budget(&budget)?;
        usage.record_chain_depth(state.grant_chain(Some(&auth.grant_leaf))?.len());
        let mut domains: BTreeSet<_> = checked
            .heads
            .iter()
            .map(|head| head.domain.clone())
            .collect();
        domains.extend(budget.dependencies.keys().cloned());
        domains.insert(PolicyDomain::Account(account.account.clone()));
        for domain in domains {
            match domain {
                PolicyDomain::Account(id) => usage.record_verified_object(
                    state
                        .account_history(&id)
                        .and_then(|history| history.last())
                        .ok_or(AuthorizationError::PolicyConflict)?,
                )?,
                PolicyDomain::Space(id) => usage.record_verified_object(
                    state
                        .space_history(&id)
                        .and_then(|history| history.last())
                        .ok_or(AuthorizationError::PolicyConflict)?,
                )?,
            }
        }
        let space_genesis = state
            .space_history(&selected.space)
            .and_then(|history| history.first())
            .ok_or(AuthorizationError::PolicyConflict)?;
        usage.record_verified_object(space_genesis)?;
        usage.record_metadata(&(cursor_json, now))?;
        let directory = DeviceDirectory {
            account: account.account.clone(),
            head,
            devices,
            next_cursor,
        };
        usage.record_metadata(&directory)?;
        if let Some(counts) = SignatureVerification::current_counts() {
            usage.record_actual_signature_counts(counts);
        }
        usage.enforce_limits()?;
        Ok(directory)
    };
    serde_json::to_string(&verify().map_err(|e| e.to_string())?)
        .map_err(|_| "directory encoding failed".into())
}

#[cfg(test)]
mod tests {
    use super::super::bootstrap::*;
    use super::*;
    const NOW: f64 = 10_000.0;
    const END: u64 = 1_000_000;

    fn key(byte: u8) -> Identity {
        Identity::from_secret_hex(&hex::encode([byte; 32])).unwrap()
    }

    fn authorizations(
        state: &VerifiedPolicySet,
        pins: &[PolicyPin],
        bundle: &ClientAuthorizationBundle,
        resource: &ResourceRef,
    ) -> (VerifiedClientAuthorization, VerifiedClientAuthorization) {
        use client_policy_import::*;
        let own = key(3);
        let node = key(4);
        let audience = state
            .grant(&bundle.grant_leaf)
            .unwrap()
            .payload
            .audience
            .clone();
        let account = match &state
            .find_device(&DeviceId::from_key(&PublicKey::of(&own)))
            .unwrap()
            .payload
            .subject
        {
            Actor::Account(account) => account.clone(),
            _ => unreachable!(),
        };
        let selection = AuthorizationSelection {
            resource: resource.clone(),
            audience,
            grant_leaf: bundle.grant_leaf.clone(),
        };
        let request_json = build_client_policy_read_request(
            account.as_str(),
            resource.space.as_str(),
            &own,
            &node.public_key_hex(),
            &serde_json::to_string(&pins).unwrap(),
            &serde_json::to_string(&Some(&selection)).unwrap(),
            NOW,
            NOW + 60_000.0,
        )
        .unwrap();
        let request: Signed<ClientPolicyReadRequest> = serde_json::from_str(&request_json).unwrap();
        let current = verify_client_policy_read(
            state,
            &request,
            &PublicKey::of(&node),
            &PublicKey::of(&own),
            NOW as u64,
        )
        .unwrap();
        let snapshot = Signed::sign(
            ClientPolicySnapshot {
                version: AUTH_VERSION,
                request_hash: request.hash().unwrap(),
                snapshot_id: Nonce::random().unwrap(),
                saved_pins_hash: client_policy_saved_pins_hash(pins).unwrap(),
                target_heads: current.heads().to_vec(),
                selection: selection.clone(),
                source_node_public_key: PublicKey::of(&node),
                issued_at_ms: NOW as u64,
                expires_at_ms: NOW as u64 + 60_000,
            },
            &node,
        )
        .unwrap();
        let mut import = begin_client_policy_import(
            &serde_json::to_string(&pins).unwrap(),
            &request_json,
            &serde_json::to_string(&snapshot).unwrap(),
            &own,
            &node.public_key_hex(),
            NOW,
        )
        .unwrap();
        let mut previous = snapshot.hash().unwrap();
        let mut request_cursor = None;
        for (index, entry) in bundle.journal.iter().enumerate() {
            let complete = index + 1 == bundle.journal.len();
            let page = Signed::sign(
                ClientPolicyPage {
                    version: AUTH_VERSION,
                    request_hash: request.hash().unwrap(),
                    snapshot_id: snapshot.payload.snapshot_id.clone(),
                    target_heads_hash: client_policy_target_heads_hash(current.heads()).unwrap(),
                    page_index: index as u64,
                    request_cursor: request_cursor.clone(),
                    previous_page_hash: previous,
                    entry: Some(entry.clone()),
                    next_cursor: if complete {
                        None
                    } else {
                        Some(ObjectHash::body(&("native-test-cursor", index)).unwrap())
                    },
                    complete,
                },
                &node,
            )
            .unwrap();
            previous = page.hash().unwrap();
            request_cursor = page.payload.next_cursor.clone();
            import
                .append_page(&serde_json::to_string(&page).unwrap(), NOW)
                .unwrap();
        }
        let witness = Signed::sign(
            ClientPolicyWitness {
                version: AUTH_VERSION,
                request_hash: request.hash().unwrap(),
                snapshot_id: snapshot.payload.snapshot_id.clone(),
                target_heads: current.heads().to_vec(),
                final_page_hash: previous,
                page_count: bundle.journal.len() as u64,
                entry_count: bundle.journal.len() as u64,
                snapshot_hash: snapshot.hash().unwrap(),
                source_node_public_key: PublicKey::of(&node),
                issued_at_ms: NOW as u64,
                expires_at_ms: NOW as u64 + 60_000,
            },
            &node,
        )
        .unwrap();
        let authorization = import
            .finish(
                &serde_json::to_string(&witness).unwrap(),
                &serde_json::to_string(&selection).unwrap(),
                NOW,
            )
            .unwrap();
        let confirmation_request_json =
            build_client_policy_auth_confirm_request(&authorization, &own, NOW, NOW + 30_000.0)
                .unwrap();
        let confirmation_request: Signed<ClientPolicyAuthConfirmRequest> =
            serde_json::from_str(&confirmation_request_json).unwrap();
        verify_client_policy_auth_confirm(
            state,
            &request,
            &snapshot,
            &witness,
            &confirmation_request,
            &PublicKey::of(&node),
            &PublicKey::of(&own),
            NOW as u64,
        )
        .unwrap();
        let confirmation = Signed::sign(
            ClientPolicyAuthConfirmation {
                version: AUTH_VERSION,
                context: confirmation_request.payload.context.clone(),
                confirmation_request_hash: confirmation_request.hash().unwrap(),
                selection,
                target_heads: current.heads().to_vec(),
                device_public_key: PublicKey::of(&own),
                source_node_public_key: PublicKey::of(&node),
                accepted_at_ms: NOW as u64,
            },
            &node,
        )
        .unwrap();
        let confirmed = confirm_client_policy_authorization(
            &authorization,
            &confirmation_request_json,
            &serde_json::to_string(&confirmation).unwrap(),
            &node.public_key_hex(),
            NOW,
        )
        .unwrap();
        (authorization, confirmed)
    }

    struct Fixture {
        state: VerifiedPolicySet,
        pins: Vec<PolicyPin>,
        bundle: ClientAuthorizationBundle,
        account: AccountId,
        resource: ResourceRef,
    }

    impl Fixture {
        fn new(nodes: u8) -> Self {
            let nonce = Nonce::from_bytes([9; 32]);
            let claim = verify_node_claim(
                &create_node_claim(&key(4), nonce.as_str(), NOW, NOW + 100_000.0).unwrap(),
                &key(4).public_key_hex(),
                nonce.as_str(),
                NOW,
            )
            .unwrap();
            let prepared =
                prepare_account_creation(&key(1), &key(2), &key(3), &claim, END as f64, NOW)
                    .unwrap();
            let possession = sign_node_possession(
                &prepared.node_request_json().unwrap(),
                &key(4),
                nonce.as_str(),
                NOW,
            )
            .unwrap();
            let created = finish_account_creation(&prepared, &key(1), &possession, NOW).unwrap();
            let pins: Vec<PolicyPin> = serde_json::from_str(&created.pins_json().unwrap()).unwrap();
            let bundle: ClientAuthorizationBundle =
                serde_json::from_str(&created.bundle_json().unwrap()).unwrap();
            let state = replay_policies(&pins, &bundle.journal).unwrap();
            let account = state.accounts.keys().next().unwrap().clone();
            let project = state
                .resources
                .values()
                .find(|record| record.payload.kind == ResourceKind::Project)
                .unwrap();
            let resource = ResourceRef {
                space: project.payload.space.clone(),
                resource: project.payload.resource.clone(),
            };
            let mut fixture = Self {
                state,
                pins,
                bundle,
                account,
                resource,
            };
            for index in 0..nodes {
                let node = key(20 + index);
                let public_key = PublicKey::of(&node);
                let mut certificate = DeviceCertificate {
                    version: AUTH_VERSION,
                    owner: PolicyDomain::Account(fixture.account.clone()),
                    subject: Actor::Service(ServiceId::from_key(&public_key)),
                    device: DeviceId::from_key(&public_key),
                    public_key,
                    uses: vec![DeviceUse::Noise, DeviceUse::ServiceInvocation],
                    possession: None,
                    enrollment_grant: None,
                    issued_under: fixture
                        .state
                        .head(&PolicyDomain::Account(fixture.account.clone()))
                        .unwrap()
                        .clone(),
                    nonce: Nonce::from_bytes([20 + index; 32]),
                    not_before_ms: if index == 11 {
                        NOW as u64 + 5_000
                    } else {
                        NOW as u64
                    },
                    expires_at_ms: if index == 0 { NOW as u64 + 1_000 } else { END },
                };
                certificate.prove_possession(&node).unwrap();
                let certificate = Signed::sign(certificate, &key(1)).unwrap();
                let hash = fixture
                    .state
                    .install_device(certificate.clone(), certificate.payload.not_before_ms)
                    .unwrap();
                fixture
                    .bundle
                    .journal
                    .push(JournalEntry::Device(certificate));
                let mut document = fixture.state.account(&fixture.account).unwrap().clone();
                document.devices.push(hash);
                document.devices.sort();
                fixture.update(document);
            }
            fixture
        }

        fn update(&mut self, mut document: AccountDocument) {
            document.previous = self
                .state
                .head(&PolicyDomain::Account(self.account.clone()))
                .cloned();
            document.sequence += 1;
            let signed = Signed::sign(document, &key(1)).unwrap();
            self.state.install_account(signed.clone()).unwrap();
            self.bundle.journal.push(JournalEntry::Account(signed));
            for pin in &mut self.pins {
                pin.minimum_head = self.state.head(&pin.domain).cloned();
            }
        }

        fn authorizations(&self) -> (VerifiedClientAuthorization, VerifiedClientAuthorization) {
            authorizations(&self.state, &self.pins, &self.bundle, &self.resource)
        }
    }

    fn page(auth: &VerifiedClientAuthorization, cursor: &str, now: f64) -> serde_json::Value {
        let signatures = SignatureVerification::new();
        let encoded = signatures
            .run(|| verify_account_device_directory(auth, cursor, now))
            .unwrap();
        assert!(encoded.len() <= MAX_PROOF_BYTES);
        assert!(signatures.counts().actual_verifications > 0);
        assert!(signatures.counts().actual_verifications <= MAX_PROOF_SIGNATURES);
        let page: serde_json::Value = serde_json::from_str(&encoded).unwrap();
        assert!(page["devices"].as_array().unwrap().len() <= 1);
        page
    }

    #[test]
    fn twelve_nodes_use_confirmed_finite_directory_without_replaying_public_history() {
        let fixture = Fixture::new(12);
        let (unconfirmed, auth) = fixture.authorizations();
        assert!(verify_account_device_directory(&unconfirmed, "null", NOW).is_err());
        let mut cursor = "null".to_owned();
        let mut certificates = Vec::new();
        let mut statuses = BTreeMap::new();
        loop {
            let result = page(&auth, &cursor, NOW + 2_000.0);
            assert_eq!(
                result["account"],
                serde_json::to_value(&fixture.account).unwrap()
            );
            assert_eq!(
                result["head"],
                serde_json::to_value(
                    fixture
                        .state
                        .head(&PolicyDomain::Account(fixture.account.clone()))
                        .unwrap()
                )
                .unwrap()
            );
            for device in result["devices"].as_array().unwrap() {
                let certificate: Signed<DeviceCertificate> =
                    serde_json::from_value(device["certificate"].clone()).unwrap();
                certificates.push(certificate.hash().unwrap());
                statuses.insert(
                    certificate.payload.device,
                    device["status"].as_str().unwrap().to_owned(),
                );
            }
            if result["next_cursor"].is_null() {
                break;
            }
            assert_ne!(cursor, result["next_cursor"].to_string());
            cursor = result["next_cursor"].to_string();
        }
        assert_eq!(
            certificates,
            fixture.state.account(&fixture.account).unwrap().devices
        );
        assert_eq!(certificates.len(), 14);
        assert_eq!(
            statuses[&DeviceId::from_key(&PublicKey::of(&key(20)))],
            "expired"
        );
        assert_eq!(
            statuses[&DeviceId::from_key(&PublicKey::of(&key(31)))],
            "pending"
        );
        assert_eq!(statuses[&auth.scope.device_id], "active");
        let last = serde_json::to_string(certificates.last().unwrap()).unwrap();
        assert!(
            page(&auth, &last, NOW)["devices"]
                .as_array()
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn directory_rejects_invalid_cursor_expired_manager_and_missing_management_right() {
        let mut fixture = Fixture::new(0);
        let (_, auth) = fixture.authorizations();
        let legacy = verify_client_authorization(
            &serde_json::to_string(&fixture.bundle).unwrap(),
            &serde_json::to_string(&fixture.pins).unwrap(),
            fixture.account.as_str(),
            fixture.resource.space.as_str(),
            &key(3).public_key_hex(),
            NOW,
        )
        .unwrap();
        assert!(verify_account_device_directory(&legacy, "null", NOW).is_err());
        let wrong = serde_json::to_string(&ObjectHash::body(&"unregistered").unwrap()).unwrap();
        for cursor in [wrong.as_str(), "{}", "0", "\"invalid\""] {
            assert!(verify_account_device_directory(&auth, cursor, NOW).is_err());
        }
        let padded_cursor = format!("{}null", " ".repeat(MAX_PROOF_BYTES - 4));
        assert_eq!(
            verify_account_device_directory(&auth, &padded_cursor, NOW).unwrap_err(),
            AuthorizationError::BudgetExceeded.to_string(),
        );
        assert!(verify_account_device_directory(&auth, "null", END as f64).is_err());
        let mut document = fixture.state.account(&fixture.account).unwrap().clone();
        document
            .enrollment
            .iter_mut()
            .find(|entry| entry.device == auth.scope.device_id)
            .unwrap()
            .expires_at_ms = NOW as u64 + 1_000;
        fixture.update(document);
        let (_, expired_manager) = fixture.authorizations();
        assert!(verify_account_device_directory(&expired_manager, "null", NOW + 2_000.0).is_err());
        let mut document = fixture.state.account(&fixture.account).unwrap().clone();
        document.enrollment.clear();
        fixture.update(document);
        let (_, denied) = fixture.authorizations();
        assert!(verify_account_device_directory(&denied, "null", NOW).is_err());
    }

    #[test]
    fn refreshed_directory_excludes_revoked_certificates_and_retains_enroll_or_revoke_right() {
        let mut fixture = Fixture::new(1);
        let revoked = DeviceId::from_key(&PublicKey::of(&key(20)));
        let revoked_hash = fixture.state.find_device(&revoked).unwrap().hash().unwrap();
        let mut document = fixture.state.account(&fixture.account).unwrap().clone();
        document.devices.retain(|hash| hash != &revoked_hash);
        document.revoked_devices.push(revoked.clone());
        document.revoked_devices.sort();
        let manager = document
            .enrollment
            .iter_mut()
            .find(|entry| entry.device == DeviceId::from_key(&PublicKey::of(&key(3))))
            .unwrap();
        manager.rights = vec![AccountRight::RevokeDevice];
        fixture.update(document);
        let (_, auth) = fixture.authorizations();
        let first = page(&auth, "null", NOW);
        assert_eq!(page(&auth, "null", NOW), first);
        let cursor = first["next_cursor"].to_string();
        let second = page(&auth, &cursor, NOW);
        assert!(second["next_cursor"].is_null());
        for result in [first, second] {
            let certificate: Signed<DeviceCertificate> =
                serde_json::from_value(result["devices"][0]["certificate"].clone()).unwrap();
            assert_ne!(certificate.payload.device, revoked);
        }
        assert!(
            verify_account_device_directory(
                &auth,
                &serde_json::to_string(&revoked_hash).unwrap(),
                NOW,
            )
            .is_err()
        );
    }
}
