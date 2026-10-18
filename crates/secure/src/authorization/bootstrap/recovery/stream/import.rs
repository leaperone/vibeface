use super::*;
use crate::authorization::policy_import::{BoundedPolicyImport, VerifiedPolicySnapshot};

#[derive(Clone, Debug)]
#[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen)]
pub struct RecoveryContextImport {
    request: RecoveryReadRequest,
    snapshot: Signed<RecoveryContextSnapshot>,
    policy: BoundedPolicyImport,
    saved_floors: BTreeMap<ObjectHash, crate::webauthn::AuthenticatorState>,
    page_count: u64,
    entry_count: u64,
    previous_hash: ObjectHash,
    next_cursor: Option<ObjectHash>,
    complete: bool,
    last_page: Option<Signed<RecoveryContextPage>>,
    last_usage: Option<RecoveryProofCounts>,
}

#[derive(Clone, Debug)]
#[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen)]
pub struct VerifiedRecoveryPolicyContext {
    request: RecoveryReadRequest,
    snapshot: Signed<RecoveryContextSnapshot>,
    witness: Signed<RecoveryContextWitness>,
    policy: VerifiedPolicySnapshot,
    usage: RecoveryProofCounts,
}

pub(super) fn validate_snapshot(
    request: &RecoveryReadRequest,
    snapshot: &Signed<RecoveryContextSnapshot>,
    now: u64,
) -> Result<()> {
    request.validate_saved_pins()?;
    let value = &snapshot.payload;
    require(
        value.version == AUTH_VERSION
            && value.kind == request.kind()
            && value.request_hash == request.request_hash()?
            && value.saved_pins_hash == request.saved_pins_hash()?
            && value.source_node_public_key == *request.source_node_public_key()
            && snapshot.issuer == *request.source_node_public_key()
            && value.target_heads.len() == 2
            && value
                .target_heads
                .windows(2)
                .all(|pair| pair[0].domain < pair[1].domain)
            && value.target_heads.iter().all(|head| {
                request.saved_pins().iter().any(|pin| {
                    pin.domain == head.domain
                        && pin.minimum_head.as_ref().is_some_and(|saved| {
                            (head.authority_epoch, head.sequence)
                                >= (saved.authority_epoch, saved.sequence)
                        })
                })
            })
            && value.issued_at_ms >= request.issued_at_ms()
            && value.expires_at_ms <= request.expires_at_ms(),
        "recovery snapshot changed the original request, trust or target heads",
    )?;
    validate_window(value.issued_at_ms, value.expires_at_ms, now)?;
    match request {
        RecoveryReadRequest::Account(value) => validate_claim(
            &snapshot.payload.claim,
            &value.payload.node_public_key,
            &value.payload.nonce,
            now,
        )?,
        RecoveryReadRequest::CrossNode(value) => {
            require(
                snapshot.payload.claim == value.payload.target_claim,
                "cross-node snapshot changed the target claim",
            )?;
            validate_claim(
                &value.payload.target_claim,
                &value.payload.target_claim.payload.node_public_key,
                &value.payload.target_claim.payload.claim_nonce,
                now,
            )?;
        }
    }
    snapshot.verify_signature()
}

fn begin(
    request: RecoveryReadRequest,
    snapshot: Signed<RecoveryContextSnapshot>,
    saved_floors: BTreeMap<ObjectHash, crate::webauthn::AuthenticatorState>,
    now: u64,
) -> Result<RecoveryContextImport> {
    validate_snapshot(&request, &snapshot, now)?;
    let required = request
        .saved_pins()
        .iter()
        .map(|pin| {
            pin.minimum_head
                .clone()
                .ok_or(AuthorizationError::InvalidProof("missing exact saved head"))
        })
        .collect::<Result<Vec<_>>>()?;
    let policy = BoundedPolicyImport::new(request.saved_pins().to_vec(), required)?;
    let previous_hash = snapshot.hash()?;
    Ok(RecoveryContextImport {
        request,
        snapshot,
        policy,
        saved_floors,
        page_count: 0,
        entry_count: 0,
        previous_hash,
        next_cursor: None,
        complete: false,
        last_page: None,
        last_usage: None,
    })
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen(js_name = beginRecoveryContextImport))]
pub fn begin_recovery_context_import(
    material_json: &str,
    original_request_json: &str,
    snapshot_json: &str,
    now_ms: f64,
) -> crate::Result<RecoveryContextImport> {
    let scope = crate::signature_verification::ScopedSignatureVerification::new();
    let start =
        || -> Result<RecoveryContextImport> {
            require(
                material_json
                    .len()
                    .saturating_add(original_request_json.len())
                    .saturating_add(snapshot_json.len())
                    <= MAX_PROOF_BYTES,
                "recovery import begin carrier exceeds 64 KiB",
            )?;
            let material = read_material(material_json)?;
            validate_material(&material)?;
            let state = material_state(&material)?;
            let signed: Signed<RecoveryContextRequest> =
                decode_signed(original_request_json.as_bytes())?;
            let request = RecoveryReadRequest::Account(signed);
            require(
                request.saved_pins() == material.pins
                    && request.account() == &material.scope.account_id
                    && request.space() == &material.scope.space_id
                    && request.source_node_public_key() == &material.entry.node_public_key,
                "recovery import changed the saved private material",
            )?;
            let now = timestamp(now_ms)?;
            request.verify(&state, &material.entry.node_public_key, now)?;
            let snapshot: Signed<RecoveryContextSnapshot> =
                decode_signed(snapshot_json.as_bytes())?;
            let floors =
                state
                    .account(&material.scope.account_id)
                    .ok_or(AuthorizationError::Revoked)?
                    .authenticators
                    .iter()
                    .map(|hash| {
                        let record = state.authenticators.get(hash).ok_or(
                            AuthorizationError::InvalidProof("missing saved authenticator"),
                        )?;
                        Ok((
                            hash.clone(),
                            state
                                .authenticator_states
                                .get(hash)
                                .copied()
                                .unwrap_or(record.payload.initial_state),
                        ))
                    })
                    .collect::<Result<BTreeMap<_, _>>>()?;
            let mut import = begin(request, snapshot, floors, now)?;
            let mut usage = RecoveryProofUsage::new(
                material_json
                    .len()
                    .saturating_add(original_request_json.len())
                    .saturating_add(snapshot_json.len()),
            )?;
            usage.merge_usage(&checkpoint::material_proof_usage(&material)?);
            usage.record_request(&import.request)?;
            usage.record_snapshot(&import.snapshot)?;
            import.last_usage = Some(usage.finish()?);
            require(
                scope.counts().actual_verifications <= MAX_PROOF_SIGNATURES,
                "recovery import signature budget exceeded",
            )?;
            Ok(import)
        };
    start().map_err(|error| error.to_string())
}

/// C may import a new claimed account without receiving private recovery material.
pub fn begin_cross_node_recovery_policy_import(
    original_request_json: &str,
    snapshot_json: &str,
    local_target_public_key: &str,
    expected_claim_nonce: &str,
    now_ms: u64,
) -> Result<RecoveryContextImport> {
    let _scope = crate::signature_verification::ScopedSignatureVerification::new();
    require(
        original_request_json
            .len()
            .saturating_add(snapshot_json.len())
            <= MAX_PROOF_BYTES,
        "cross-node recovery import begin carrier exceeds 64 KiB",
    )?;
    let request: Signed<CrossNodeRecoveryRequest> =
        decode_signed(original_request_json.as_bytes())?;
    validate_claim(
        &request.payload.target_claim,
        &PublicKey::parse(local_target_public_key)?,
        &Nonce::parse(expected_claim_nonce)?,
        now_ms,
    )?;
    request.verify_signature()?;
    let snapshot: Signed<RecoveryContextSnapshot> = decode_signed(snapshot_json.as_bytes())?;
    begin(
        RecoveryReadRequest::CrossNode(request),
        snapshot,
        BTreeMap::new(),
        now_ms,
    )
}

impl RecoveryContextImport {
    fn progress(&self) -> crate::Result<String> {
        encode(
            &serde_json::json!({"page_count":self.page_count,"entry_count":self.entry_count,
            "cursor":self.next_cursor,"complete":self.complete}),
        )
    }
    pub fn append(&mut self, page: Signed<RecoveryContextPage>, now: u64) -> Result<()> {
        let _scope = crate::signature_verification::ScopedSignatureVerification::new();
        require(
            canonical(&page)?.len() <= MAX_PROOF_BYTES,
            "recovery page carrier exceeds 64 KiB",
        )?;
        validate_window(
            self.snapshot.payload.issued_at_ms,
            self.snapshot.payload.expires_at_ms,
            now,
        )?;
        if self.last_page.as_ref() == Some(&page) {
            let mut usage = RecoveryProofUsage::new(canonical(&page)?.len())?;
            usage.record_page_binding(&page)?;
            self.last_usage = Some(usage.finish()?);
            return Ok(());
        }
        let value = &page.payload;
        require(
            !self.complete
                && page.issuer == *self.request.source_node_public_key()
                && value.version == AUTH_VERSION
                && value.kind == self.request.kind()
                && value.request_hash == self.request.request_hash()?
                && value.snapshot_id == self.snapshot.payload.snapshot_id
                && value.target_heads_hash
                    == recovery_target_heads_hash(&self.snapshot.payload.target_heads)?
                && value.page_index == self.page_count
                && value.previous_page_hash == self.previous_hash
                && value.complete == value.next_cursor.is_none(),
            "recovery page changed its request, snapshot, order or completion",
        )?;
        page.verify_signature()?;
        let mut usage = RecoveryProofUsage::new(canonical(&page)?.len())?;
        usage.record_page(&page, self.policy.state())?;
        let mut policy = self.policy.clone();
        if let Some(entry) = &value.entry {
            policy.apply_entry(entry.clone())?;
            let account_domain = PolicyDomain::Account(self.request.account().clone());
            let saved = self
                .request
                .saved_pins()
                .iter()
                .find(|pin| pin.domain == account_domain)
                .and_then(|pin| pin.minimum_head.as_ref())
                .ok_or(AuthorizationError::InvalidProof(
                    "missing saved Account head",
                ))?;
            if policy.state().head(&account_domain) == Some(saved) {
                for (hash, floor) in &self.saved_floors {
                    let record = policy.state().authenticators.get(hash).ok_or(
                        AuthorizationError::InvalidProof(
                            "public history omitted a saved authenticator",
                        ),
                    )?;
                    require(
                        policy
                            .state()
                            .authenticator_states
                            .get(hash)
                            .copied()
                            .unwrap_or(record.payload.initial_state)
                            == *floor,
                        "public history changed the exact saved authenticator floor",
                    )?;
                }
            }
        }
        let usage = usage.finish()?;
        let page_count = self
            .page_count
            .checked_add(1)
            .ok_or(AuthorizationError::BudgetExceeded)?;
        let entry_count = self
            .entry_count
            .checked_add(u64::from(value.entry.is_some()))
            .ok_or(AuthorizationError::BudgetExceeded)?;
        let previous_hash = page.hash()?;
        self.page_count = page_count;
        self.entry_count = entry_count;
        self.previous_hash = previous_hash;
        self.next_cursor = value.next_cursor.clone();
        self.complete = value.complete;
        self.last_page = Some(page);
        self.policy = policy;
        self.last_usage = Some(usage);
        Ok(())
    }
    pub fn finish_native(
        self,
        witness: Signed<RecoveryContextWitness>,
        now: u64,
    ) -> Result<VerifiedRecoveryPolicyContext> {
        let _scope = crate::signature_verification::ScopedSignatureVerification::new();
        require(
            canonical(&witness)?.len() <= MAX_PROOF_BYTES,
            "recovery witness carrier exceeds 64 KiB",
        )?;
        let value = &witness.payload;
        require(
            self.complete
                && self.page_count > 0
                && witness.issuer == *self.request.source_node_public_key()
                && value.version == AUTH_VERSION
                && value.kind == self.request.kind()
                && value.request_hash == self.request.request_hash()?
                && value.snapshot_id == self.snapshot.payload.snapshot_id
                && value.target_heads == self.snapshot.payload.target_heads
                && value.final_page_hash == self.previous_hash
                && value.page_count == self.page_count
                && value.entry_count == self.entry_count
                && value.claim_hash == self.snapshot.payload.claim.hash()?
                && value.source_node_public_key == *self.request.source_node_public_key()
                && value.issued_at_ms >= self.snapshot.payload.issued_at_ms
                && value.expires_at_ms <= self.snapshot.payload.expires_at_ms,
            "recovery witness does not seal the exact completed history read",
        )?;
        validate_window(value.issued_at_ms, value.expires_at_ms, now)?;
        witness.verify_signature()?;
        let mut usage = RecoveryProofUsage::new(canonical(&witness)?.len())?;
        usage.record_witness(&witness)?;
        let policy = self.policy.finish(&value.target_heads)?;
        self.request
            .verify(policy.state(), self.request.source_node_public_key(), now)?;
        usage.record_request(&self.request)?;
        usage.record_current_source(&self.request, policy.state())?;
        let usage = usage.finish()?;
        Ok(VerifiedRecoveryPolicyContext {
            request: self.request,
            snapshot: self.snapshot,
            witness,
            policy,
            usage,
        })
    }
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen)]
impl RecoveryContextImport {
    #[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen(js_name = proofUsageJson))]
    pub fn proof_usage_json(&self) -> crate::Result<String> {
        encode(&self.last_usage)
    }
    #[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen(js_name = appendPage))]
    pub fn append_page(&mut self, page_json: &str, now_ms: f64) -> crate::Result<String> {
        require(
            page_json.len() <= MAX_PROOF_BYTES,
            "recovery page carrier exceeds 64 KiB",
        )
        .map_err(|error| error.to_string())?;
        let page = decode_signed(page_json.as_bytes()).map_err(|error| error.to_string())?;
        self.append(page, timestamp(now_ms).map_err(|error| error.to_string())?)
            .map_err(|error| error.to_string())?;
        self.progress()
    }
    #[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen(js_name = finish))]
    pub fn finish(
        self,
        witness_json: &str,
        now_ms: f64,
    ) -> crate::Result<VerifiedRecoveryPolicyContext> {
        require(
            witness_json.len() <= MAX_PROOF_BYTES,
            "recovery witness carrier exceeds 64 KiB",
        )
        .map_err(|error| error.to_string())?;
        let witness = decode_signed(witness_json.as_bytes()).map_err(|error| error.to_string())?;
        self.finish_native(
            witness,
            timestamp(now_ms).map_err(|error| error.to_string())?,
        )
        .map_err(|error| error.to_string())
    }
}

impl VerifiedRecoveryPolicyContext {
    pub fn request(&self) -> &RecoveryReadRequest {
        &self.request
    }
    pub fn snapshot(&self) -> &Signed<RecoveryContextSnapshot> {
        &self.snapshot
    }
    pub fn witness(&self) -> &Signed<RecoveryContextWitness> {
        &self.witness
    }
    pub(crate) fn verified_state(&self) -> &VerifiedPolicySet {
        self.policy.state()
    }
    pub(crate) fn verified_entries(&self) -> &[JournalEntry] {
        self.policy.journal()
    }
    pub(crate) fn policy(&self) -> &VerifiedPolicySnapshot {
        &self.policy
    }
    pub fn reference(&self) -> Result<RecoveryContextReference> {
        Ok(RecoveryContextReference {
            snapshot_id: self.snapshot.payload.snapshot_id.clone(),
            request_hash: self.request.request_hash()?,
            snapshot_hash: self.snapshot.hash()?,
            witness_hash: self.witness.hash()?,
        })
    }
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen)]
impl VerifiedRecoveryPolicyContext {
    #[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen(js_name = proofUsageJson))]
    pub fn proof_usage_json(&self) -> crate::Result<String> {
        encode(&self.usage)
    }
    #[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen(js_name = claimJson))]
    pub fn claim_json(&self) -> crate::Result<String> {
        encode(&self.snapshot.payload.claim)
    }
    #[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen(js_name = contextJson))]
    pub fn context_json(&self) -> crate::Result<String> {
        encode(
            &serde_json::json!({"reference":self.reference().map_err(|error| error.to_string())?,
            "account":self.request.account(),"space":self.request.space(),
            "new_device_public_key":self.request.new_device_public_key(),"node_public_key":self.request.source_node_public_key(),
            "heads":self.snapshot.payload.target_heads}),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::signature_verification::SignatureVerification;

    const NOW: f64 = 20_000.0;
    const EXPIRY: f64 = 60_000.0;
    fn key(byte: u8) -> Identity {
        Identity::from_secret_hex(&hex::encode([byte; 32])).unwrap()
    }

    #[test]
    fn twelve_node_history_imports_with_exact_heads_and_bounded_individual_calls() {
        let original = super::super::super::tests::account_with_enrolled_nodes(12);
        let material = build_account_recovery_material(
            &original,
            &key(1),
            &key(2),
            "https://node.example",
            10_000.0,
        )
        .unwrap();
        let request_json = context::build_recovery_context_request(
            &material,
            &key(50),
            &key(4).public_key_hex(),
            NOW,
            EXPIRY,
        )
        .unwrap();
        let request: Signed<RecoveryContextRequest> = serde_json::from_str(&request_json).unwrap();
        let read = RecoveryReadRequest::Account(request.clone());
        let claim_json =
            create_node_claim(&key(4), request.payload.nonce.as_str(), NOW, EXPIRY).unwrap();
        let claim: Signed<NodeClaim> = serde_json::from_str(&claim_json).unwrap();
        let snapshot = Signed::sign(
            RecoveryContextSnapshot {
                version: AUTH_VERSION,
                kind: RecoveryContextKind::Account,
                request_hash: read.request_hash().unwrap(),
                snapshot_id: Nonce::from_bytes([90; 32]),
                saved_pins_hash: read.saved_pins_hash().unwrap(),
                target_heads: original
                    .pins
                    .iter()
                    .filter_map(|pin| pin.minimum_head.clone())
                    .collect(),
                source_node_public_key: PublicKey::of(&key(4)),
                claim,
                issued_at_ms: NOW as u64,
                expires_at_ms: EXPIRY as u64,
            },
            &key(4),
        )
        .unwrap();
        let snapshot_json = encode(&snapshot).unwrap();
        let signatures = SignatureVerification::new();
        let mut import = signatures
            .run(|| begin_recovery_context_import(&material, &request_json, &snapshot_json, NOW))
            .unwrap();
        assert!(signatures.counts().actual_verifications <= MAX_PROOF_SIGNATURES);
        let mut stages = vec![
            serde_json::json!({"stage":"begin","usage":import.last_usage,"actual":signatures.counts()}),
        ];
        let mut previous = snapshot.hash().unwrap();
        let mut pages = Vec::new();
        for (index, entry) in original.bundle.journal.iter().enumerate() {
            let complete = index + 1 == original.bundle.journal.len();
            let page = Signed::sign(
                RecoveryContextPage {
                    version: AUTH_VERSION,
                    kind: RecoveryContextKind::Account,
                    request_hash: read.request_hash().unwrap(),
                    snapshot_id: snapshot.payload.snapshot_id.clone(),
                    target_heads_hash: recovery_target_heads_hash(&snapshot.payload.target_heads)
                        .unwrap(),
                    page_index: index as u64,
                    previous_page_hash: previous,
                    entry: Some(entry.clone()),
                    next_cursor: if complete {
                        None
                    } else {
                        Some(ObjectHash::body(&("cursor", index)).unwrap())
                    },
                    complete,
                },
                &key(4),
            )
            .unwrap();
            previous = page.hash().unwrap();
            pages.push(page);
        }
        let witness = Signed::sign(
            RecoveryContextWitness {
                version: AUTH_VERSION,
                kind: RecoveryContextKind::Account,
                request_hash: read.request_hash().unwrap(),
                snapshot_id: snapshot.payload.snapshot_id.clone(),
                target_heads: snapshot.payload.target_heads.clone(),
                final_page_hash: previous,
                page_count: pages.len() as u64,
                entry_count: pages.len() as u64,
                claim_hash: snapshot.payload.claim.hash().unwrap(),
                source_node_public_key: PublicKey::of(&key(4)),
                issued_at_ms: NOW as u64,
                expires_at_ms: EXPIRY as u64,
            },
            &key(4),
        )
        .unwrap();
        assert!(
            import
                .clone()
                .finish_native(witness.clone(), NOW as u64)
                .is_err()
        );
        let mut wrong = pages[0].payload.clone();
        wrong.snapshot_id = Nonce::from_bytes([91; 32]);
        assert!(
            import
                .append(Signed::sign(wrong, &key(4)).unwrap(), NOW as u64)
                .is_err()
        );
        assert_eq!(import.page_count, 0);
        let mut wrong = pages[0].payload.clone();
        wrong.entry = original
            .bundle
            .journal
            .iter()
            .rev()
            .find(|entry| matches!(entry, JournalEntry::AccountCommit(_)))
            .cloned();
        assert!(
            import
                .append(Signed::sign(wrong, &key(4)).unwrap(), NOW as u64)
                .is_err()
        );
        assert_eq!(import.page_count, 0);
        for (index, page) in pages.iter().enumerate() {
            let signatures = SignatureVerification::new();
            signatures
                .run(|| import.append_page(&encode(page).unwrap(), NOW))
                .unwrap();
            let usage = import.last_usage.as_ref().unwrap();
            assert!(usage.carrier_bytes <= MAX_PROOF_BYTES);
            assert!(usage.proof.unique_signed_objects <= MAX_PROOF_OBJECTS);
            assert!(usage.proof.unique_signatures <= MAX_PROOF_SIGNATURES);
            assert!(usage.proof.max_chain_depth <= MAX_DELEGATION_DEPTH);
            assert!(signatures.counts().actual_verifications <= MAX_PROOF_SIGNATURES);
            stages.push(serde_json::json!({"stage":"page","index":index,"usage":usage,"actual":signatures.counts()}));
            if index == 0 {
                let signatures = SignatureVerification::new();
                signatures
                    .run(|| import.append_page(&encode(page).unwrap(), NOW))
                    .unwrap();
                assert_eq!(signatures.counts().actual_verifications, 0);
                assert_eq!(import.page_count, 1);
            }
        }
        let signatures = SignatureVerification::new();
        let context = signatures
            .run(|| import.finish(&encode(&witness).unwrap(), NOW))
            .unwrap();
        assert_eq!(context.policy.heads(), snapshot.payload.target_heads);
        assert_eq!(context.policy.journal(), original.bundle.journal);
        assert!(signatures.counts().actual_verifications <= MAX_PROOF_SIGNATURES);
        stages.push(serde_json::json!({"stage":"finish","usage":context.usage,"actual":signatures.counts()}));
        let max_actual = stages
            .iter()
            .filter_map(|stage| stage["actual"]["actual_verifications"].as_u64())
            .max()
            .unwrap();
        let max_objects = stages
            .iter()
            .filter_map(|stage| stage["usage"]["proof"]["unique_signed_objects"].as_u64())
            .max()
            .unwrap();
        let max_bytes = stages
            .iter()
            .filter_map(|stage| stage["usage"]["carrier_bytes"].as_u64())
            .max()
            .unwrap();
        println!(
            "bounded recovery import evidence: {}",
            serde_json::json!({"root_approved_node_enrollments":12,
            "full_public_bundle_bytes":original.bundle_json().unwrap().len(),"pages":pages.len(),
            "max_actual":max_actual,"max_objects":max_objects,"max_carrier_bytes":max_bytes})
        );
        if let Ok(path) = std::env::var("VIBEFACE_BOUNDED_RECOVERY_VECTOR") {
            let value = serde_json::json!({"now":NOW,"expiry":EXPIRY,"material":material,
                "request":request,"snapshot":snapshot,"pages":pages,"witness":witness,
                "expected":{"scope":original.authorization.scope,"heads":context.policy.heads()},"stages":stages});
            let mut options = std::fs::OpenOptions::new();
            options.write(true).create(true).truncate(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            let mut file = options.open(path).unwrap();
            std::io::Write::write_all(&mut file, &serde_json::to_vec(&value).unwrap()).unwrap();
        }
    }
}
