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
    last_proof: Option<RecoveryProofUsage>,
}

#[derive(Clone, Debug)]
#[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen)]
pub struct VerifiedRecoveryPolicyContext {
    request: RecoveryReadRequest,
    snapshot: Signed<RecoveryContextSnapshot>,
    witness: Signed<RecoveryContextWitness>,
    policy: VerifiedPolicySnapshot,
    usage: RecoveryProofCounts,
    proof_usage: RecoveryProofUsage,
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
        last_proof: None,
    })
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen(js_name = beginRecoveryContextImport))]
pub fn begin_recovery_context_import(
    material_json: &str,
    original_request_json: &str,
    snapshot_json: &str,
    now_ms: f64,
) -> crate::Result<RecoveryContextImport> {
    begin_private_recovery_context_import(
        material_json,
        original_request_json,
        snapshot_json,
        RecoveryContextKind::Account,
        now_ms,
    )
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen(js_name = beginCrossNodeRecoveryContextImport))]
pub fn begin_cross_node_recovery_context_import(
    material_json: &str,
    original_request_json: &str,
    snapshot_json: &str,
    now_ms: f64,
) -> crate::Result<RecoveryContextImport> {
    begin_private_recovery_context_import(
        material_json,
        original_request_json,
        snapshot_json,
        RecoveryContextKind::CrossNode,
        now_ms,
    )
}

fn begin_private_recovery_context_import(
    material_json: &str,
    original_request_json: &str,
    snapshot_json: &str,
    kind: RecoveryContextKind,
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
            let request = match kind {
                RecoveryContextKind::Account => {
                    RecoveryReadRequest::Account(decode_signed(original_request_json.as_bytes())?)
                }
                RecoveryContextKind::CrossNode => {
                    RecoveryReadRequest::CrossNode(decode_signed(original_request_json.as_bytes())?)
                }
            };
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
            import.last_usage = Some(usage.clone().finish()?);
            import.last_proof = Some(usage);
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
    let mut import = begin(
        RecoveryReadRequest::CrossNode(request),
        snapshot,
        BTreeMap::new(),
        now_ms,
    )?;
    let mut usage = RecoveryProofUsage::new(
        original_request_json
            .len()
            .saturating_add(snapshot_json.len()),
    )?;
    usage.record_request(&import.request)?;
    usage.record_snapshot(&import.snapshot)?;
    import.last_usage = Some(usage.clone().finish()?);
    import.last_proof = Some(usage);
    Ok(import)
}

impl RecoveryContextImport {
    pub fn record_proof_usage_into(&self, usage: &mut RecoveryProofUsage) {
        if let Some(proof) = &self.last_proof {
            usage.merge(proof);
        }
    }
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
            self.last_usage = Some(usage.clone().finish()?);
            self.last_proof = Some(usage);
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
        let proof_usage = usage;
        let usage = proof_usage.clone().finish()?;
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
        self.last_proof = Some(proof_usage);
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
        let proof_usage = usage;
        let usage = proof_usage.clone().finish()?;
        Ok(VerifiedRecoveryPolicyContext {
            request: self.request,
            snapshot: self.snapshot,
            witness,
            policy,
            usage,
            proof_usage,
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
    pub fn record_proof_usage_into(&self, usage: &mut RecoveryProofUsage) {
        usage.merge(&self.proof_usage);
    }
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
pub(super) mod tests {
    use super::*;
    use crate::signature_verification::SignatureVerification;

    const NOW: f64 = 20_000.0;
    const EXPIRY: f64 = 60_000.0;
    fn key(byte: u8) -> Identity {
        Identity::from_secret_hex(&hex::encode([byte; 32])).unwrap()
    }

    pub(in crate::authorization::bootstrap::recovery) fn measured<T, E: std::fmt::Debug>(
        stages: &mut Vec<serde_json::Value>,
        name: &str,
        run: impl FnOnce() -> std::result::Result<T, E>,
    ) -> T {
        let signatures = SignatureVerification::new();
        let result = signatures.run(run).unwrap();
        assert!(signatures.counts().actual_verifications <= MAX_PROOF_SIGNATURES);
        stages.push(serde_json::json!({"stage":name,"actual":signatures.counts()}));
        result
    }

    pub(in crate::authorization::bootstrap::recovery) fn confirmed_client(
        created: &CreatedAccount,
        client: &Identity,
        node: &Identity,
        now: u64,
        expiry: u64,
        stages: &mut Vec<serde_json::Value>,
    ) -> VerifiedClientAuthorization {
        confirmed_client_with_evidence(created, client, node, now, expiry, stages).0
    }

    pub(in crate::authorization::bootstrap::recovery) fn confirmed_client_with_evidence(
        created: &CreatedAccount,
        client: &Identity,
        node: &Identity,
        now: u64,
        expiry: u64,
        stages: &mut Vec<serde_json::Value>,
    ) -> (VerifiedClientAuthorization, serde_json::Value) {
        use crate::authorization::client_policy_import as auth;
        let resource = created.authorization.resources.first().unwrap();
        let selection = auth::AuthorizationSelection {
            resource: ResourceRef {
                space: resource.payload.space.clone(),
                resource: resource.payload.resource.clone(),
            },
            audience: created.authorization.audience.clone(),
            grant_leaf: created.bundle.grant_leaf.clone(),
        };
        let request_json = measured(stages, "normal_read_request", || {
            auth::build_client_policy_read_request(
                created.authorization.scope.account_id.as_str(),
                created.authorization.scope.space_id.as_str(),
                client,
                &node.public_key_hex(),
                &encode(&created.pins).unwrap(),
                &encode(&Some(selection.clone())).unwrap(),
                now as f64,
                expiry as f64,
            )
        });
        let request: Signed<auth::ClientPolicyReadRequest> =
            serde_json::from_str(&request_json).unwrap();
        let verified = measured(stages, "normal_source_read", || {
            auth::verify_client_policy_read(
                &created.authorization.policy_snapshot,
                &request,
                &PublicKey::of(node),
                &PublicKey::of(client),
                now,
            )
        });
        let snapshot = Signed::sign(
            auth::ClientPolicySnapshot {
                version: AUTH_VERSION,
                request_hash: request.hash().unwrap(),
                snapshot_id: Nonce::from_bytes([94; 32]),
                saved_pins_hash: auth::client_policy_saved_pins_hash(&created.pins).unwrap(),
                target_heads: verified.heads().to_vec(),
                selection: selection.clone(),
                source_node_public_key: PublicKey::of(node),
                issued_at_ms: now,
                expires_at_ms: expiry,
            },
            node,
        )
        .unwrap();
        let mut import = measured(stages, "normal_import_begin", || {
            auth::begin_client_policy_import(
                &encode(&created.pins).unwrap(),
                &request_json,
                &encode(&snapshot).unwrap(),
                client,
                &node.public_key_hex(),
                now as f64,
            )
        });
        let mut previous = snapshot.hash().unwrap();
        let mut pages = Vec::new();
        for (index, entry) in created.bundle.journal.iter().enumerate() {
            let complete = index + 1 == created.bundle.journal.len();
            let page = Signed::sign(
                auth::ClientPolicyPage {
                    version: AUTH_VERSION,
                    request_hash: request.hash().unwrap(),
                    snapshot_id: snapshot.payload.snapshot_id.clone(),
                    target_heads_hash: auth::client_policy_target_heads_hash(verified.heads())
                        .unwrap(),
                    page_index: index as u64,
                    previous_page_hash: previous,
                    entry: Some(entry.clone()),
                    next_cursor: (!complete)
                        .then(|| ObjectHash::body(&("normal-cursor", index)).unwrap()),
                    complete,
                },
                node,
            )
            .unwrap();
            previous = page.hash().unwrap();
            measured(stages, "normal_import_page", || {
                import.append_page(&encode(&page).unwrap(), now as f64)
            });
            pages.push(page);
        }
        let witness = Signed::sign(
            auth::ClientPolicyWitness {
                version: AUTH_VERSION,
                request_hash: request.hash().unwrap(),
                snapshot_id: snapshot.payload.snapshot_id.clone(),
                target_heads: verified.heads().to_vec(),
                final_page_hash: previous,
                page_count: created.bundle.journal.len() as u64,
                entry_count: created.bundle.journal.len() as u64,
                snapshot_hash: snapshot.hash().unwrap(),
                source_node_public_key: PublicKey::of(node),
                issued_at_ms: now,
                expires_at_ms: expiry,
            },
            node,
        )
        .unwrap();
        let imported = measured(stages, "normal_import_finish", || {
            import.finish(
                &encode(&witness).unwrap(),
                &encode(&selection).unwrap(),
                now as f64,
            )
        });
        let confirm_json = measured(stages, "normal_confirm_request", || {
            auth::build_client_policy_auth_confirm_request(
                &imported,
                client,
                now as f64,
                expiry as f64,
            )
        });
        let confirm: Signed<auth::ClientPolicyAuthConfirmRequest> =
            serde_json::from_str(&confirm_json).unwrap();
        let confirmation = measured(stages, "normal_source_confirm", || {
            auth::verify_client_policy_auth_confirm(
                &created.authorization.policy_snapshot,
                &request,
                &snapshot,
                &witness,
                &confirm,
                &PublicKey::of(node),
                &PublicKey::of(client),
                now,
            )?;
            Signed::sign(
                auth::ClientPolicyAuthConfirmation {
                    version: AUTH_VERSION,
                    context: confirm.payload.context.clone(),
                    confirmation_request_hash: confirm.hash()?,
                    selection: selection.clone(),
                    target_heads: verified.heads().to_vec(),
                    device_public_key: PublicKey::of(client),
                    source_node_public_key: PublicKey::of(node),
                    accepted_at_ms: now,
                },
                node,
            )
        });
        let confirmed = measured(stages, "normal_client_confirm", || {
            auth::confirm_client_policy_authorization(
                &imported,
                &confirm_json,
                &encode(&confirmation).unwrap(),
                &node.public_key_hex(),
                now as f64,
            )
        });
        (
            confirmed,
            serde_json::json!({"now":now,"expiry":expiry,"request":request,
                "snapshot":snapshot,"pages":pages,"witness":witness,
                "selection":selection,"confirm_request":confirm,"confirmation":confirmation}),
        )
    }

    fn assert_usage(usage: &RecoveryProofCounts) {
        assert!(usage.carrier_bytes <= MAX_PROOF_BYTES);
        assert!(usage.proof.unique_signed_objects <= MAX_PROOF_OBJECTS);
        assert!(usage.proof.unique_signatures <= MAX_PROOF_SIGNATURES);
        assert!(usage.proof.max_chain_depth <= MAX_DELEGATION_DEPTH);
    }

    pub(in crate::authorization::bootstrap::recovery) fn import_account_history(
        before: &CreatedAccount,
        material: &str,
        client: &Identity,
        now: f64,
        expiry: f64,
        snapshot_nonce: u8,
        stages: &mut Vec<serde_json::Value>,
    ) -> (
        VerifiedRecoveryPolicyContext,
        Vec<Signed<RecoveryContextPage>>,
    ) {
        let request_json = measured(stages, "read_request", || {
            context::build_recovery_context_request(
                material,
                client,
                &key(4).public_key_hex(),
                now,
                expiry,
            )
        });
        let request: Signed<RecoveryContextRequest> = serde_json::from_str(&request_json).unwrap();
        let read = RecoveryReadRequest::Account(request.clone());
        let claim_json =
            create_node_claim(&key(4), request.payload.nonce.as_str(), now, expiry).unwrap();
        let claim: Signed<NodeClaim> = serde_json::from_str(&claim_json).unwrap();
        let snapshot = Signed::sign(
            RecoveryContextSnapshot {
                version: AUTH_VERSION,
                kind: RecoveryContextKind::Account,
                request_hash: read.request_hash().unwrap(),
                snapshot_id: Nonce::from_bytes([snapshot_nonce; 32]),
                saved_pins_hash: read.saved_pins_hash().unwrap(),
                target_heads: before
                    .pins
                    .iter()
                    .filter_map(|pin| pin.minimum_head.clone())
                    .collect(),
                source_node_public_key: PublicKey::of(&key(4)),
                claim,
                issued_at_ms: now as u64,
                expires_at_ms: expiry as u64,
            },
            &key(4),
        )
        .unwrap();
        let snapshot_json = encode(&snapshot).unwrap();
        let signatures = SignatureVerification::new();
        let mut import = signatures
            .run(|| begin_recovery_context_import(material, &request_json, &snapshot_json, now))
            .unwrap();
        assert!(signatures.counts().actual_verifications <= MAX_PROOF_SIGNATURES);
        assert_usage(import.last_usage.as_ref().unwrap());
        stages.push(serde_json::json!({"stage":"begin","usage":import.last_usage,"actual":signatures.counts()}));
        let mut previous = snapshot.hash().unwrap();
        let mut pages = Vec::new();
        for (index, entry) in before.bundle.journal.iter().enumerate() {
            let complete = index + 1 == before.bundle.journal.len();
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
                issued_at_ms: now as u64,
                expires_at_ms: expiry as u64,
            },
            &key(4),
        )
        .unwrap();
        assert!(
            import
                .clone()
                .finish_native(witness.clone(), now as u64)
                .is_err()
        );
        let mut wrong = pages[0].payload.clone();
        wrong.snapshot_id = Nonce::from_bytes([snapshot_nonce + 1; 32]);
        assert!(
            import
                .append(Signed::sign(wrong, &key(4)).unwrap(), now as u64)
                .is_err()
        );
        assert_eq!(import.page_count, 0);
        let mut wrong = pages[0].payload.clone();
        wrong.entry = before
            .bundle
            .journal
            .iter()
            .rev()
            .find(|entry| matches!(entry, JournalEntry::AccountCommit(_)))
            .cloned();
        assert!(
            import
                .append(Signed::sign(wrong, &key(4)).unwrap(), now as u64)
                .is_err()
        );
        assert_eq!(import.page_count, 0);
        for (index, page) in pages.iter().enumerate() {
            let signatures = SignatureVerification::new();
            signatures
                .run(|| import.append_page(&encode(page).unwrap(), now))
                .unwrap();
            let usage = import.last_usage.as_ref().unwrap();
            assert_usage(usage);
            assert!(signatures.counts().actual_verifications <= MAX_PROOF_SIGNATURES);
            stages.push(serde_json::json!({"stage":"page","index":index,"usage":usage,"actual":signatures.counts()}));
            if index == 0 {
                let signatures = SignatureVerification::new();
                signatures
                    .run(|| import.append_page(&encode(page).unwrap(), now))
                    .unwrap();
                assert_eq!(signatures.counts().actual_verifications, 0);
                assert_eq!(import.page_count, 1);
            }
        }
        let signatures = SignatureVerification::new();
        let context = signatures
            .run(|| import.finish(&encode(&witness).unwrap(), now))
            .unwrap();
        assert_eq!(context.policy.heads(), snapshot.payload.target_heads);
        assert_eq!(context.policy.journal(), before.bundle.journal);
        assert!(signatures.counts().actual_verifications <= MAX_PROOF_SIGNATURES);
        assert_usage(&context.usage);
        stages.push(serde_json::json!({"stage":"finish","usage":context.usage,"actual":signatures.counts()}));
        (context, pages)
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
        let mut stages = Vec::new();
        let (context, pages) =
            import_account_history(&original, &material, &key(50), NOW, EXPIRY, 90, &mut stages);
        let read = context.request().clone();
        let RecoveryReadRequest::Account(request) = read.clone() else {
            unreachable!()
        };
        let snapshot = context.snapshot().clone();
        let witness = context.witness().clone();
        let signatures = SignatureVerification::new();
        let prepared = signatures
            .run(|| {
                prepare_account_recovery(&material, &context, &key(51), &key(50), 1_000_000.0, NOW)
            })
            .unwrap();
        assert!(signatures.counts().actual_verifications <= MAX_PROOF_SIGNATURES);
        stages.push(serde_json::json!({"stage":"prepare","actual":signatures.counts()}));
        let approved: StreamedRecoveryNodeRequest =
            serde_json::from_str(&prepared.node_request_json().unwrap()).unwrap();
        let prefix_commitment = ObjectHash::body(&(
            "fixed-native-accepted-prefix",
            original.bundle.journal.len(),
            ObjectHash::body(original.bundle.journal.last().unwrap()).unwrap(),
        ))
        .unwrap();
        let signatures = SignatureVerification::new();
        let (possession, source_approval) = signatures
            .run(|| {
                approve_recovery_source_proposal(
                    &approved,
                    &read,
                    &snapshot,
                    &witness,
                    &original.authorization.policy_snapshot,
                    &key(4),
                    read.nonce(),
                    &prefix_commitment,
                    NOW as u64,
                )
            })
            .unwrap();
        assert!(signatures.counts().actual_verifications <= MAX_PROOF_SIGNATURES);
        stages.push(serde_json::json!({"stage":"possession","actual":signatures.counts()}));
        let signatures = SignatureVerification::new();
        let recovered = signatures
            .run(|| finish_account_recovery(&prepared, &key(51), possession.as_str(), NOW))
            .unwrap();
        assert!(recovered.authorization.imported_snapshot.is_some());
        assert!(signatures.counts().actual_verifications <= MAX_PROOF_SIGNATURES);
        stages.push(serde_json::json!({"stage":"recovery_finish","actual":signatures.counts()}));
        let commit_json = prepared.commit_json(&recovered).unwrap();
        let commit: StreamedRecoveryCommitRequest = serde_json::from_str(&commit_json).unwrap();
        assert!(
            verify_recovery_source_proof_prefix(
                &read,
                &snapshot,
                &witness,
                &original.authorization.policy_snapshot,
                &[],
                &PublicKey::of(&key(4)),
                NOW as u64,
            )
            .is_err()
        );
        let signatures = SignatureVerification::new();
        let committed = signatures.run(|| {
            let source = verify_recovery_source_proposal_approval(
                &source_approval,
                &read,
                &approved.context,
                &ObjectHash::body(&approved)?,
                &approved.request.account_recovery,
                &possession,
                &original.authorization.policy_snapshot,
                &PublicKey::of(&key(4)),
                &prefix_commitment,
                NOW as u64,
            )?;
            verify_recovery_stream_commit_with_source_approval(
                &commit,
                &approved,
                &read,
                &original.authorization.policy_snapshot,
                &source,
                NOW as u64,
            )
        });
        eprintln!(
            "recovery commit actual {}",
            serde_json::to_string(&signatures.counts()).unwrap()
        );
        let committed = committed.unwrap();
        assert_eq!(
            committed.authorization().scope,
            recovered.authorization.scope
        );
        assert!(signatures.counts().actual_verifications <= MAX_PROOF_SIGNATURES);
        stages.push(serde_json::json!({"stage":"commit","usage":committed.proof_usage(),"actual":signatures.counts()}));
        let signatures = SignatureVerification::new();
        let next_material = signatures
            .run(|| {
                build_recovered_account_recovery_material(
                    &recovered,
                    &key(51),
                    &material,
                    "https://node.example",
                    NOW,
                )
            })
            .unwrap();
        assert!(next_material.len() <= MAX_RECOVERY_MATERIAL_BYTES);
        stages.push(serde_json::json!({"stage":"backup","material_bytes":next_material.len(),"actual":signatures.counts()}));
        let signatures = SignatureVerification::new();
        let pending = signatures
            .run(|| {
                build_account_recovery_pending(
                    &prepared,
                    &recovered,
                    &key(51),
                    &key(50),
                    &material,
                    "https://node.example",
                    NOW,
                )
            })
            .unwrap();
        let pending_json = zeroize::Zeroizing::new(pending.pending_material_json().unwrap());
        let signatures_restore = SignatureVerification::new();
        let restored = signatures_restore
            .run(|| restore_account_recovery_pending(&pending_json, EXPIRY + 1.0))
            .unwrap();
        assert_eq!(restored.commit_request_json().unwrap(), commit_json);
        assert_eq!(
            restored.client_identity().unwrap().public_key_hex(),
            key(50).public_key_hex()
        );
        stages.push(serde_json::json!({"stage":"pending","private_bytes":pending_json.len(),"actual":signatures.counts()}));
        stages.push(
            serde_json::json!({"stage":"pending_restore","actual":signatures_restore.counts()}),
        );
        let current = confirmed_client(
            &recovered,
            &key(50),
            &key(4),
            EXPIRY as u64 + 1,
            EXPIRY as u64 + 30_000,
            &mut stages,
        );
        let completed_heads = committed.committed_heads();
        let receipt = Signed::sign(
            RecoveryCompletionReceipt {
                version: AUTH_VERSION,
                context: commit.context.clone(),
                request_hash: commit.request_hash.clone(),
                input_hash: ObjectHash::body(&commit).unwrap(),
                committed_account_head: completed_heads[0].clone(),
                committed_space_head: completed_heads[1].clone(),
                grant_leaf: commit.grant_leaf.clone(),
                new_device_public_key: commit.new_device_public_key.clone(),
                source_node_public_key: PublicKey::of(&key(4)),
                accepted_at_ms: NOW as u64,
            },
            &key(4),
        )
        .unwrap();
        let confirmed = measured(&mut stages, "pending_confirm_current_auth", || {
            restored.confirm_completion(&encode(&receipt).unwrap(), &current, EXPIRY + 1.0)
        });
        assert_eq!(
            confirmed.authorization().scope_json().unwrap(),
            current.scope_json().unwrap()
        );
        assert_eq!(
            confirmed.client_identity().unwrap().public_key_hex(),
            key(50).public_key_hex()
        );
        let second_now = EXPIRY + 2.0;
        let second_expiry = second_now + 30_000.0;
        let mut second_stages = Vec::new();
        let (second_context, second_pages) = import_account_history(
            &recovered,
            &next_material,
            &key(53),
            second_now,
            second_expiry,
            92,
            &mut second_stages,
        );
        assert_eq!(second_context.request().saved_pins(), recovered.pins);
        assert_eq!(second_context.policy.heads(), committed.committed_heads());
        assert_eq!(
            second_context
                .verified_state()
                .account(&recovered.authorization.scope.account_id)
                .unwrap()
                .current_root,
            PublicKey::of(&key(51))
        );
        let second_prepared = measured(&mut second_stages, "prepare", || {
            prepare_account_recovery(
                &next_material,
                &second_context,
                &key(52),
                &key(53),
                1_000_000.0,
                second_now,
            )
        });
        let second_approved: StreamedRecoveryNodeRequest =
            serde_json::from_str(&second_prepared.node_request_json().unwrap()).unwrap();
        let second_prefix_commitment = ObjectHash::body(&(
            "fixed-native-accepted-prefix",
            recovered.bundle.journal.len(),
            ObjectHash::body(recovered.bundle.journal.last().unwrap()).unwrap(),
        ))
        .unwrap();
        let (second_possession, second_source_approval) =
            measured(&mut second_stages, "possession", || {
                approve_recovery_source_proposal(
                    &second_approved,
                    second_context.request(),
                    second_context.snapshot(),
                    second_context.witness(),
                    &recovered.authorization.policy_snapshot,
                    &key(4),
                    second_context.request().nonce(),
                    &second_prefix_commitment,
                    second_now as u64,
                )
            });
        let second_recovered = measured(&mut second_stages, "recovery_finish", || {
            finish_account_recovery(
                &second_prepared,
                &key(52),
                second_possession.as_str(),
                second_now,
            )
        });
        let second_commit: StreamedRecoveryCommitRequest =
            serde_json::from_str(&second_prepared.commit_json(&second_recovered).unwrap()).unwrap();
        let second_committed = measured(&mut second_stages, "commit", || {
            let source = verify_recovery_source_proposal_approval(
                &second_source_approval,
                second_context.request(),
                &second_approved.context,
                &ObjectHash::body(&second_approved)?,
                &second_approved.request.account_recovery,
                &second_possession,
                &recovered.authorization.policy_snapshot,
                &PublicKey::of(&key(4)),
                &second_prefix_commitment,
                second_now as u64,
            )?;
            verify_recovery_stream_commit_with_source_approval(
                &second_commit,
                &second_approved,
                second_context.request(),
                &recovered.authorization.policy_snapshot,
                &source,
                second_now as u64,
            )
        });
        assert_usage(second_committed.proof_usage());
        second_stages.last_mut().unwrap()["usage"] =
            serde_json::to_value(second_committed.proof_usage()).unwrap();
        assert_eq!(
            second_committed.authorization().scope,
            second_recovered.authorization.scope
        );
        assert_eq!(
            second_committed.committed_heads(),
            second_recovered
                .pins
                .iter()
                .filter_map(|pin| pin.minimum_head.clone())
                .collect::<Vec<_>>()
        );
        assert!(second_recovered.authorization.imported_snapshot.is_some());
        assert_eq!(
            second_recovered
                .authorization
                .policy_snapshot
                .account(&recovered.authorization.scope.account_id)
                .unwrap()
                .current_root,
            PublicKey::of(&key(52))
        );
        let second_material = measured(&mut second_stages, "backup", || {
            build_recovered_account_recovery_material(
                &second_recovered,
                &key(52),
                &next_material,
                "https://node.example",
                second_now,
            )
        });
        let mut material_usage = RecoveryProofUsage::new(second_material.len()).unwrap();
        material_usage.merge_usage(
            &recovery_material_usage(&read_material(&second_material).unwrap()).unwrap(),
        );
        let material_usage = material_usage.finish().unwrap();
        assert_usage(&material_usage);
        second_stages.last_mut().unwrap()["usage"] = serde_json::to_value(material_usage).unwrap();
        let third_request_json = measured(&mut second_stages, "third_read_request", || {
            context::build_recovery_context_request(
                &second_material,
                &key(54),
                &key(4).public_key_hex(),
                second_now + 1.0,
                second_expiry,
            )
        });
        let third_request: Signed<RecoveryContextRequest> =
            serde_json::from_str(&third_request_json).unwrap();
        assert_eq!(third_request.payload.saved_pins, second_recovered.pins);
        assert_ne!(third_request.payload.saved_pins, recovered.pins);
        assert_eq!(third_request.payload.account, request.payload.account);
        assert_eq!(third_request.payload.space, request.payload.space);
        assert_eq!(
            third_request.payload.node_public_key,
            PublicKey::of(&key(4))
        );
        assert_eq!(third_request.issuer, PublicKey::of(&key(2)));
        let max_actual = stages
            .iter()
            .chain(&second_stages)
            .filter_map(|stage| stage["actual"]["actual_verifications"].as_u64())
            .max()
            .unwrap();
        let max_objects = stages
            .iter()
            .chain(&second_stages)
            .filter_map(|stage| stage["usage"]["proof"]["unique_signed_objects"].as_u64())
            .max()
            .unwrap();
        let max_bytes = stages
            .iter()
            .chain(&second_stages)
            .filter_map(|stage| stage["usage"]["carrier_bytes"].as_u64())
            .max()
            .unwrap();
        println!(
            "bounded recovery import evidence: {}",
            serde_json::json!({"root_approved_node_enrollments":12,
            "full_public_bundle_bytes":original.bundle_json().unwrap().len(),"pages":pages.len(),
            "recovery_rounds":2,"second_round_pages":second_pages.len(),"third_read_ready":true,
            "max_actual":max_actual,"max_objects":max_objects,"max_carrier_bytes":max_bytes})
        );
        if let Ok(path) = std::env::var("VIBEFACE_BOUNDED_RECOVERY_VECTOR") {
            let value = serde_json::json!({"now":NOW,"expiry":EXPIRY,"material":material,
                "request":request,"snapshot":snapshot,"pages":pages,"witness":witness,
                "expected":{"scope":original.authorization.scope,"heads":context.policy.heads()},
                "node_request":approved,"node_possession":possession,"source_approval":source_approval,"prefix_commitment":prefix_commitment,"commit":commit,
                "next_material":next_material,"pending":*pending_json,"stages":stages,
                "second_round":{"now":second_now,"expiry":second_expiry,
                    "request":match second_context.request() { RecoveryReadRequest::Account(request) => request, _ => unreachable!() },
                    "snapshot":second_context.snapshot(),"pages":second_pages,"witness":second_context.witness(),
                    "node_request":second_approved,"node_possession":second_possession,
                    "source_approval":second_source_approval,"prefix_commitment":second_prefix_commitment,
                    "commit":second_commit,"next_material":second_material,
                    "third_read_request":third_request,"stages":second_stages}});
            let mut options = std::fs::OpenOptions::new();
            options.write(true).create(true).truncate(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            let mut file = options.open(path).unwrap();
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                file.set_permissions(std::fs::Permissions::from_mode(0o600))
                    .unwrap();
            }
            std::io::Write::write_all(&mut file, &serde_json::to_vec(&value).unwrap()).unwrap();
        }
    }
}
