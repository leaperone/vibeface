//! Private recovery resumption retains both backups and never restores an authorization brand.

use super::*;
use crate::authorization::client_policy_import::{
    require_confirmed_client_policy, revalidate_client_policy_authorization,
};
use crate::authorization::policy_proof::policy_entry_payload_usage;
use zeroize::Zeroize;

const FORMAT: &str = "vibeface-account-recovery-stream-pending";

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PendingMaterial {
    format: String,
    version: u16,
    before_material: RecoveryMaterial,
    material: RecoveryMaterial,
    client_secret_hex: String,
    read: RecoveryReadRequest,
    snapshot: Signed<RecoveryContextSnapshot>,
    witness: Signed<RecoveryContextWitness>,
    node_request: StreamedRecoveryNodeRequest,
    commit_request: StreamedRecoveryCommitRequest,
    input_hash: ObjectHash,
}

impl Drop for PendingMaterial {
    fn drop(&mut self) {
        self.client_secret_hex.zeroize();
    }
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen)]
pub struct PendingAccountRecovery {
    material: PendingMaterial,
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen)]
pub struct ConfirmedAccountRecovery {
    authorization: VerifiedClientAuthorization,
    recovery_material: String,
    client_secret_hex: String,
}

impl Drop for ConfirmedAccountRecovery {
    fn drop(&mut self) {
        self.recovery_material.zeroize();
        self.client_secret_hex.zeroize();
    }
}

fn bounded<T: Serialize>(value: &T) -> Result<()> {
    require(
        canonical(value)?.len() <= MAX_PROOF_BYTES,
        "recovery public carrier exceeds 64 KiB",
    )
}

fn committed_heads(input: &StreamedRecoveryCommitRequest) -> Result<[PolicyStamp; 2]> {
    let account = input
        .journal_delta
        .iter()
        .rev()
        .find_map(|entry| match entry {
            JournalEntry::Account(value) => Some(account_stamp(value)),
            _ => None,
        });
    let space = input
        .journal_delta
        .iter()
        .rev()
        .find_map(|entry| match entry {
            JournalEntry::Space(value) => Some(space_stamp(value)),
            _ => None,
        });
    Ok([
        account.ok_or(AuthorizationError::PolicyConflict)??,
        space.ok_or(AuthorizationError::PolicyConflict)??,
    ])
}

fn verify_pending(pending: &PendingMaterial, now: u64) -> Result<()> {
    bounded(&pending.commit_request)?;
    bounded(&pending.node_request)?;
    bounded(&pending.read)?;
    bounded(&pending.snapshot)?;
    bounded(&pending.witness)?;
    require(
        zeroize::Zeroizing::new(canonical(&pending.before_material)?).len()
            <= MAX_RECOVERY_MATERIAL_BYTES
            && zeroize::Zeroizing::new(canonical(&pending.material)?).len()
                <= MAX_RECOVERY_MATERIAL_BYTES,
        "pending recovery backup exceeds 64 KiB",
    )?;
    let recovery = validate_material(&pending.before_material)?;
    validate_material(&pending.material)?;
    let before = material_state(&pending.before_material)?;
    let after = material_state(&pending.material)?;
    let RecoveryReadRequest::Account(read) = &pending.read else {
        return Err(AuthorizationError::InvalidProof(
            "ordinary pending recovery cannot use a cross-node read",
        ));
    };
    let input = &pending.commit_request;
    let request = &pending.node_request.request;
    let r = &read.payload;
    let before_account = before
        .account(&r.account)
        .ok_or(AuthorizationError::Revoked)?;
    let at = request.node_certificate.not_before_ms;
    let client = Identity::from_secret_hex(&pending.client_secret_hex)
        .map_err(|_| AuthorizationError::InvalidProof("invalid pending recovery client"))?;
    let heads = committed_heads(input)?;
    pending.read.validate_saved_pins()?;
    super::import::validate_snapshot(&pending.read, &pending.snapshot, at)?;
    let witness = &pending.witness.payload;
    let reference = RecoveryContextReference {
        snapshot_id: pending.snapshot.payload.snapshot_id.clone(),
        request_hash: read.hash()?,
        snapshot_hash: pending.snapshot.hash()?,
        witness_hash: pending.witness.hash()?,
    };
    require(
        pending.format == FORMAT
            && pending.version == AUTH_VERSION
            && at <= now
            && pending.input_hash == ObjectHash::body(input)?
            && input.context == reference
            && pending.node_request.context == reference
            && input.request_hash == ObjectHash::body(request)?
            && input.expected_account_head == request.expected_account_head
            && input.expected_space_head == request.expected_space_head
            && pending.snapshot.payload.target_heads
                == [
                    input.expected_account_head.clone(),
                    input.expected_space_head.clone(),
                ]
            && input.journal_delta.first()
                == Some(&JournalEntry::Account(request.account_recovery.clone()))
            && input.claim_nonce == r.nonce
            && request.node_certificate.nonce == r.nonce
            && request.claim == pending.snapshot.payload.claim
            && input.new_device_public_key == r.new_device_public_key
            && PublicKey::of(&client) == r.new_device_public_key
            && read.issuer == PublicKey::of(&recovery)
            && request.account_recovery.issuer == read.issuer
            && pending.before_material.pins == r.saved_pins
            && pending.before_material.scope.account_id == r.account
            && pending.before_material.scope.space_id == r.space
            && pending.before_material.entry.node_public_key == r.node_public_key
            && pending.material.entry.node_public_key == r.node_public_key
            && pending.material.scope.account_id == r.account
            && pending.material.scope.space_id == r.space
            && pending.material.scope.device_id == DeviceId::from_key(&r.new_device_public_key)
            && pending.material.bundle.grant_leaf == input.grant_leaf
            && heads[0].domain == PolicyDomain::Account(r.account.clone())
            && heads[1].domain == PolicyDomain::Space(r.space.clone())
            && after.head(&heads[0].domain) == Some(&heads[0])
            && after.head(&heads[1].domain) == Some(&heads[1])
            && after.account(&r.account).is_some_and(|account| {
                account.current_root == request.account_recovery.payload.current_root
                    && account.recovery == before_account.recovery
            })
            && pending.material.pins.len() == 2
            && pending
                .material
                .pins
                .iter()
                .zip(heads.iter())
                .all(|(pin, head)| {
                    pin.domain == head.domain
                        && pin.minimum_head.as_ref() == Some(head)
                        && r.saved_pins.iter().any(|saved| {
                            saved.domain == pin.domain && saved.genesis_root == pin.genesis_root
                        })
                })
            && pending.witness.issuer == r.node_public_key
            && witness.version == AUTH_VERSION
            && witness.kind == RecoveryContextKind::Account
            && witness.request_hash == read.hash()?
            && witness.snapshot_id == reference.snapshot_id
            && witness.target_heads == pending.snapshot.payload.target_heads
            && witness.claim_hash == request.claim.hash()?
            && witness.source_node_public_key == r.node_public_key
            && witness.page_count > 0
            && witness.issued_at_ms >= pending.snapshot.payload.issued_at_ms
            && witness.expires_at_ms <= pending.snapshot.payload.expires_at_ms,
        "pending recovery changed its original operation, identity or saved backups",
    )?;
    validate_window(r.issued_at_ms, r.expires_at_ms, at)?;
    validate_window(witness.issued_at_ms, witness.expires_at_ms, at)?;
    read.verify_signature()?;
    request.account_recovery.verify_signature()?;
    pending.witness.verify_signature()?;
    let mut usage = RecoveryProofUsage::new(canonical(input)?.len())?;
    usage.record_request(&pending.read)?;
    usage.record_snapshot(&pending.snapshot)?;
    usage.record_witness(&pending.witness)?;
    for material in [&pending.before_material, &pending.material] {
        usage.merge_usage(&recovery_material_usage(material)?);
    }
    for entry in &input.journal_delta {
        usage.merge_usage(&policy_entry_payload_usage(entry)?);
    }
    usage.finish()?;
    Ok(())
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen(js_name = buildAccountRecoveryPending))]
pub fn build_account_recovery_pending(
    prepared: &PreparedAccountRecovery,
    created: &CreatedAccount,
    next_root: &Identity,
    next_client: &Identity,
    previous_material_json: &str,
    endpoint: &str,
    now_ms: f64,
) -> crate::Result<PendingAccountRecovery> {
    let _scope = crate::signature_verification::ScopedSignatureVerification::new();
    let build = || -> Result<_> {
        let context =
            prepared
                .verified_context
                .as_ref()
                .ok_or(AuthorizationError::InvalidProof(
                    "pending recovery requires a completed streamed context",
                ))?;
        require(
            PublicKey::of(next_client) == prepared.client_certificate.payload.public_key,
            "pending recovery changed its original client",
        )?;
        let commit_request: StreamedRecoveryCommitRequest =
            serde_json::from_str(&prepared.commit_json(created).map_err(|_| {
                AuthorizationError::InvalidProof("invalid original recovery commit")
            })?)
            .map_err(|_| AuthorizationError::InvalidProof("invalid streamed recovery commit"))?;
        let next_material = zeroize::Zeroizing::new(
            build_recovered_account_recovery_material(
                created,
                next_root,
                previous_material_json,
                endpoint,
                now_ms,
            )
            .map_err(|_| {
                AuthorizationError::InvalidProof("could not preserve recovered account backup")
            })?,
        );
        let material = PendingMaterial {
            format: FORMAT.to_owned(),
            version: AUTH_VERSION,
            before_material: read_material(previous_material_json)?,
            material: read_material(&next_material)?,
            client_secret_hex: next_client.secret_hex(),
            read: context.request().clone(),
            snapshot: context.snapshot().clone(),
            witness: context.witness().clone(),
            node_request: StreamedRecoveryNodeRequest {
                context: context.reference()?,
                request: prepared.request.clone(),
            },
            input_hash: ObjectHash::body(&commit_request)?,
            commit_request,
        };
        verify_pending(&material, timestamp(now_ms)?)?;
        let pending = PendingAccountRecovery { material };
        let _encoded = zeroize::Zeroizing::new(pending.pending_material_json().map_err(|_| {
            AuthorizationError::InvalidProof("pending recovery exceeds private storage limit")
        })?);
        Ok(pending)
    };
    build().map_err(|error| error.to_string())
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen(js_name = restoreAccountRecoveryPending))]
pub fn restore_account_recovery_pending(
    pending_json: &str,
    now_ms: f64,
) -> crate::Result<PendingAccountRecovery> {
    let _scope = crate::signature_verification::ScopedSignatureVerification::new();
    let restore = || -> Result<_> {
        require(
            pending_json.len() <= MAX_RECOVERY_JOURNAL_BYTES,
            "pending recovery too large",
        )?;
        let material: PendingMaterial = serde_json::from_str(pending_json)
            .map_err(|_| AuthorizationError::InvalidProof("invalid pending recovery material"))?;
        verify_pending(&material, timestamp(now_ms)?)?;
        Ok(PendingAccountRecovery { material })
    };
    restore().map_err(|error| error.to_string())
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen)]
impl PendingAccountRecovery {
    #[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen(js_name = pendingMaterialJson))]
    pub fn pending_material_json(&self) -> crate::Result<String> {
        let mut encoded = encode(&self.material)?;
        if encoded.len() > MAX_RECOVERY_JOURNAL_BYTES {
            encoded.zeroize();
            return Err("pending recovery too large".to_owned());
        }
        Ok(encoded)
    }
    #[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen(js_name = commitRequestJson))]
    pub fn commit_request_json(&self) -> crate::Result<String> {
        bounded(&self.material.commit_request).map_err(|error| error.to_string())?;
        encode(&self.material.commit_request)
    }
    #[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen(js_name = statusRequestJson))]
    pub fn status_request_json(&self) -> crate::Result<String> {
        encode(&RecoveryCompletionStatusRequest {
            context: self.material.commit_request.context.clone(),
            input_hash: self.material.input_hash.clone(),
        })
    }
    #[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen(js_name = clientIdentity))]
    pub fn client_identity(&self) -> crate::Result<Identity> {
        Identity::from_secret_hex(&self.material.client_secret_hex)
            .map_err(|_| "invalid pending recovery client".to_owned())
    }
    #[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen(js_name = recoveryMaterialJson))]
    pub fn recovery_material_json(&self) -> crate::Result<String> {
        encode(&self.material.material)
    }
    #[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen(js_name = previousRecoveryMaterialJson))]
    pub fn previous_recovery_material_json(&self) -> crate::Result<String> {
        encode(&self.material.before_material)
    }
    #[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen(js_name = confirmCompletion))]
    pub fn confirm_completion(
        &self,
        receipt_json: &str,
        authorization: &VerifiedClientAuthorization,
        now_ms: f64,
    ) -> crate::Result<ConfirmedAccountRecovery> {
        let _scope = crate::signature_verification::ScopedSignatureVerification::new();
        let confirm = || -> Result<_> {
            require(
                receipt_json.len() <= MAX_PROOF_BYTES,
                "recovery receipt exceeds 64 KiB",
            )?;
            require_confirmed_client_policy(authorization)?;
            let current = revalidate_client_policy_authorization(
                authorization,
                &encode(&self.material.material.pins)
                    .map_err(|_| AuthorizationError::InvalidProof("invalid recovered pins"))?,
                now_ms,
            )
            .map_err(|_| {
                AuthorizationError::InvalidProof(
                    "recovery requires current confirmed client authorization",
                )
            })?;
            let snapshot = require_confirmed_client_policy(&current)?;
            let receipt: Signed<RecoveryCompletionReceipt> = serde_json::from_str(receipt_json)
                .map_err(|_| {
                    AuthorizationError::InvalidProof("invalid recovery completion receipt")
                })?;
            let now = timestamp(now_ms)?;
            let input = &self.material.commit_request;
            let heads = committed_heads(input)?;
            let r = &self.material.read;
            let value = &receipt.payload;
            require(
                value.version == AUTH_VERSION
                    && receipt.issuer == *r.source_node_public_key()
                    && value.source_node_public_key == *r.source_node_public_key()
                    && value.context == input.context
                    && value.request_hash == input.request_hash
                    && value.input_hash == self.material.input_hash
                    && value.committed_account_head == heads[0]
                    && value.committed_space_head == heads[1]
                    && value.grant_leaf == input.grant_leaf
                    && value.new_device_public_key == input.new_device_public_key
                    && self
                        .material
                        .node_request
                        .request
                        .node_certificate
                        .not_before_ms
                        <= value.accepted_at_ms
                    && value.accepted_at_ms < r.expires_at_ms()
                    && value.accepted_at_ms <= now
                    && current.scope == self.material.material.scope
                    && current.grant_leaf == input.grant_leaf
                    && current.audience_public_key == *r.source_node_public_key()
                    && current.audience
                        == (Audience {
                            service: ServiceId::from_key(r.source_node_public_key()),
                            device: DeviceId::from_key(r.source_node_public_key()),
                        })
                    && heads
                        .iter()
                        .all(|head| snapshot.state().roots.contains_key(head)),
                "recovery completion changed its original request, heads, client or source",
            )?;
            receipt.verify_signature()?;
            let mut usage = RecoveryProofUsage::new(canonical(&receipt)?.len())?;
            usage.record_completion(&receipt)?;
            usage.merge_usage(current.client_policy_usage.as_ref().ok_or(
                AuthorizationError::InvalidProof("confirmed client proof usage is missing"),
            )?);
            usage.finish()?;
            Ok(ConfirmedAccountRecovery {
                authorization: current,
                recovery_material: encode(&self.material.material)
                    .map_err(|_| AuthorizationError::InvalidProof("invalid recovered backup"))?,
                client_secret_hex: self.material.client_secret_hex.clone(),
            })
        };
        confirm().map_err(|error| error.to_string())
    }
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen)]
impl ConfirmedAccountRecovery {
    pub fn authorization(&self) -> VerifiedClientAuthorization {
        self.authorization.clone()
    }
    #[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen(js_name = recoveryMaterialJson))]
    pub fn recovery_material_json(&self) -> String {
        self.recovery_material.clone()
    }
    #[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen(js_name = clientIdentity))]
    pub fn client_identity(&self) -> crate::Result<Identity> {
        Identity::from_secret_hex(&self.client_secret_hex)
            .map_err(|_| "invalid confirmed recovery client".to_owned())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: f64 = 10_000.0;
    const RESTORE: f64 = 20_000.0;
    const END: f64 = 1_000_000.0;
    const ENDPOINT: &str = "https://node.example";

    fn key(byte: u8) -> Identity {
        Identity::from_secret_hex(&hex::encode([byte; 32])).unwrap()
    }

    fn fixture() -> (
        PendingAccountRecovery,
        CreatedAccount,
        PreparedAccountRecovery,
    ) {
        let original = super::super::super::tests::account_with_enrolled_nodes(0);
        let before =
            build_account_recovery_material(&original, &key(1), &key(2), ENDPOINT, NOW).unwrap();
        let nonce = Nonce::from_bytes([80; 32]);
        let claim = verify_node_claim(
            &create_node_claim(&key(4), nonce.as_str(), RESTORE, RESTORE + 100_000.0).unwrap(),
            &key(4).public_key_hex(),
            nonce.as_str(),
            RESTORE,
        )
        .unwrap();
        let prepared = prepare_account_recovery_from_bundle(
            &before,
            &original.bundle_json().unwrap(),
            &key(5),
            &key(6),
            &claim,
            END,
            RESTORE,
        )
        .unwrap();
        let possession = sign_recovery_node_possession(
            &prepared.node_request_json().unwrap(),
            &key(4),
            &original.pins_json().unwrap(),
            &original.bundle_json().unwrap(),
            nonce.as_str(),
            RESTORE,
        )
        .unwrap();
        let created = finish_account_recovery(&prepared, &key(5), &possession, RESTORE).unwrap();
        let after = build_recovered_account_recovery_material(
            &created,
            &key(5),
            &before,
            ENDPOINT,
            RESTORE,
        )
        .unwrap();
        let read = RecoveryReadRequest::Account(
            Signed::sign(
                context::RecoveryContextRequest {
                    version: AUTH_VERSION,
                    account: original.authorization.scope.account_id.clone(),
                    space: original.authorization.scope.space_id.clone(),
                    new_device_public_key: PublicKey::of(&key(6)),
                    node_public_key: PublicKey::of(&key(4)),
                    saved_pins: original.pins.clone(),
                    nonce: nonce.clone(),
                    issued_at_ms: RESTORE as u64,
                    expires_at_ms: RESTORE as u64 + 100_000,
                },
                &key(2),
            )
            .unwrap(),
        );
        let snapshot = Signed::sign(
            RecoveryContextSnapshot {
                version: AUTH_VERSION,
                kind: RecoveryContextKind::Account,
                request_hash: read.request_hash().unwrap(),
                snapshot_id: Nonce::from_bytes([81; 32]),
                saved_pins_hash: read.saved_pins_hash().unwrap(),
                target_heads: vec![
                    prepared.request.expected_account_head.clone(),
                    prepared.request.expected_space_head.clone(),
                ],
                source_node_public_key: PublicKey::of(&key(4)),
                claim: prepared.request.claim.clone(),
                issued_at_ms: RESTORE as u64,
                expires_at_ms: RESTORE as u64 + 100_000,
            },
            &key(4),
        )
        .unwrap();
        let witness = Signed::sign(
            RecoveryContextWitness {
                version: AUTH_VERSION,
                kind: RecoveryContextKind::Account,
                request_hash: read.request_hash().unwrap(),
                snapshot_id: snapshot.payload.snapshot_id.clone(),
                target_heads: snapshot.payload.target_heads.clone(),
                final_page_hash: ObjectHash::body(original.bundle.journal.last().unwrap()).unwrap(),
                page_count: original.bundle.journal.len() as u64,
                entry_count: original.bundle.journal.len() as u64,
                claim_hash: snapshot.payload.claim.hash().unwrap(),
                source_node_public_key: PublicKey::of(&key(4)),
                issued_at_ms: RESTORE as u64,
                expires_at_ms: RESTORE as u64 + 100_000,
            },
            &key(4),
        )
        .unwrap();
        let reference = RecoveryContextReference {
            snapshot_id: snapshot.payload.snapshot_id.clone(),
            request_hash: read.request_hash().unwrap(),
            snapshot_hash: snapshot.hash().unwrap(),
            witness_hash: witness.hash().unwrap(),
        };
        let commit_request = StreamedRecoveryCommitRequest {
            context: reference.clone(),
            request_hash: ObjectHash::body(&prepared.request).unwrap(),
            expected_account_head: prepared.request.expected_account_head.clone(),
            expected_space_head: prepared.request.expected_space_head.clone(),
            journal_delta: created.bundle.journal[prepared.journal.len()..].to_vec(),
            grant_leaf: created.bundle.grant_leaf.clone(),
            new_device_public_key: PublicKey::of(&key(6)),
            claim_nonce: nonce,
        };
        let pending = PendingAccountRecovery {
            material: PendingMaterial {
                format: FORMAT.to_owned(),
                version: AUTH_VERSION,
                before_material: read_material(&before).unwrap(),
                material: read_material(&after).unwrap(),
                client_secret_hex: key(6).secret_hex(),
                read,
                snapshot,
                witness,
                node_request: StreamedRecoveryNodeRequest {
                    context: reference,
                    request: prepared.request.clone(),
                },
                input_hash: ObjectHash::body(&commit_request).unwrap(),
                commit_request,
            },
        };
        (pending, created, prepared)
    }

    #[test]
    fn pending_restores_the_original_request_identity_and_both_backups_after_expiry() {
        let (pending, _, _) = fixture();
        let encoded = pending.pending_material_json().unwrap();
        let restored = restore_account_recovery_pending(&encoded, RESTORE + 200_000.0).unwrap();
        assert_eq!(
            restored.commit_request_json().unwrap(),
            pending.commit_request_json().unwrap()
        );
        assert_eq!(
            restored.status_request_json().unwrap(),
            pending.status_request_json().unwrap()
        );
        assert_eq!(
            restored.client_identity().unwrap().public_key_hex(),
            key(6).public_key_hex()
        );
        assert_eq!(
            restored.previous_recovery_material_json().unwrap(),
            pending.previous_recovery_material_json().unwrap()
        );
        assert_eq!(
            restored.recovery_material_json().unwrap(),
            pending.recovery_material_json().unwrap()
        );
        assert_ne!(
            restored.recovery_material_json().unwrap(),
            restored.previous_recovery_material_json().unwrap()
        );
        let status: RecoveryCompletionStatusRequest =
            serde_json::from_str(&restored.status_request_json().unwrap()).unwrap();
        assert_eq!(
            status.input_hash,
            ObjectHash::body(&restored.material.commit_request).unwrap()
        );
    }

    #[test]
    fn pending_rejects_identity_or_operation_changes_and_self_created_authorization() {
        let (pending, created, prepared) = fixture();
        let encoded = pending.pending_material_json().unwrap();
        let mut changed: PendingMaterial = serde_json::from_str(&encoded).unwrap();
        changed.client_secret_hex = key(7).secret_hex();
        assert!(restore_account_recovery_pending(&encode(&changed).unwrap(), RESTORE).is_err());
        let mut changed: PendingMaterial = serde_json::from_str(&encoded).unwrap();
        changed.commit_request.claim_nonce = Nonce::from_bytes([82; 32]);
        changed.input_hash = ObjectHash::body(&changed.commit_request).unwrap();
        assert!(restore_account_recovery_pending(&encode(&changed).unwrap(), RESTORE).is_err());
        let heads = committed_heads(&pending.material.commit_request).unwrap();
        let receipt = Signed::sign(
            RecoveryCompletionReceipt {
                version: AUTH_VERSION,
                context: pending.material.commit_request.context.clone(),
                request_hash: pending.material.commit_request.request_hash.clone(),
                input_hash: pending.material.input_hash.clone(),
                committed_account_head: heads[0].clone(),
                committed_space_head: heads[1].clone(),
                grant_leaf: pending.material.commit_request.grant_leaf.clone(),
                new_device_public_key: PublicKey::of(&key(6)),
                source_node_public_key: PublicKey::of(&key(4)),
                accepted_at_ms: RESTORE as u64,
            },
            &key(4),
        )
        .unwrap();
        assert!(
            pending
                .confirm_completion(&encode(&receipt).unwrap(), &created.authorization, RESTORE)
                .is_err()
        );
        assert!(
            build_account_recovery_pending(
                &prepared,
                &created,
                &key(5),
                &key(6),
                &pending.previous_recovery_material_json().unwrap(),
                ENDPOINT,
                RESTORE
            )
            .is_err()
        );
        assert!(
            restore_account_recovery_pending(&" ".repeat(MAX_RECOVERY_JOURNAL_BYTES + 1), RESTORE)
                .is_err()
        );
    }
}
