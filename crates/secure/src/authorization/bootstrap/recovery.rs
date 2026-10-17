//! Account recovery preserves identity and replaces device authorization atomically.

use super::*;
use zeroize::Zeroize;

const MAX_RECOVERY_MATERIAL_BYTES: usize = 65_536;
const MAX_RECOVERY_JOURNAL_BYTES: usize = 1_048_576;

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecoveryMaterial {
    pub format: String,
    pub version: u16,
    pub root_secret_hex: String,
    pub recovery_secret_hex: String,
    pub scope: ClientScope,
    pub pins: Vec<PolicyPin>,
    pub bundle: ClientAuthorizationBundle,
    pub entry: RecoveryEntry,
}
impl Drop for RecoveryMaterial {
    fn drop(&mut self) {
        self.root_secret_hex.zeroize();
        self.recovery_secret_hex.zeroize();
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RecoveryEntry {
    pub endpoint: String,
    pub node_public_key: PublicKey,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct RecoveryNodeRequest {
    claim: Signed<NodeClaim>,
    expected_account_head: PolicyStamp,
    expected_space_head: PolicyStamp,
    account_recovery: Signed<AccountDocument>,
    node_certificate: DeviceCertificate,
}

#[derive(Clone, Debug)]
#[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen)]
pub struct PreparedAccountRecovery {
    request: RecoveryNodeRequest,
    pins: Vec<PolicyPin>,
    journal: Vec<JournalEntry>,
    client_certificate: Signed<DeviceCertificate>,
    space_document: SpaceDocument,
    owner_grant: Signed<Grant>,
    service_grant: Signed<Grant>,
    new_project: Option<(ResourceId, Nonce)>,
}

#[derive(Serialize)]
struct RecoveryCommit<'a> {
    expected_account_head: &'a PolicyStamp,
    expected_space_head: &'a PolicyStamp,
    journal_delta: &'a [JournalEntry],
    claim_nonce: &'a Nonce,
    request_hash: ObjectHash,
}

fn endpoint_is_valid(endpoint: &str) -> bool {
    endpoint.len() <= 4096
        && (endpoint.starts_with("http://") || endpoint.starts_with("https://"))
        && !endpoint
            .bytes()
            .any(|b| b.is_ascii_control() || b.is_ascii_whitespace())
        && !endpoint.contains(['#', '@'])
        && endpoint
            .split_once("://")
            .is_some_and(|(_, tail)| !tail.is_empty() && !tail.starts_with('/'))
}

fn read_bundle(json: &str) -> Result<ClientAuthorizationBundle> {
    require(
        json.len() <= MAX_RECOVERY_JOURNAL_BYTES,
        "recovery journal too large",
    )?;
    let bundle: ClientAuthorizationBundle = serde_json::from_str(json)
        .map_err(|_| AuthorizationError::InvalidProof("invalid recovery journal"))?;
    require(
        bundle.journal.len() <= 128,
        "recovery journal requires checkpoint",
    )?;
    Ok(bundle)
}

fn read_material(json: &str) -> Result<RecoveryMaterial> {
    require(
        json.len() <= MAX_RECOVERY_MATERIAL_BYTES,
        "recovery material too large",
    )?;
    serde_json::from_str(json)
        .map_err(|_| AuthorizationError::InvalidProof("invalid recovery material JSON"))
}

fn validate_material(material: &RecoveryMaterial) -> Result<Identity> {
    require(
        material.format == "vibeface-account-recovery"
            && material.version == 2
            && lower_hex(&material.root_secret_hex, 32)
            && lower_hex(&material.recovery_secret_hex, 32)
            && endpoint_is_valid(&material.entry.endpoint)
            && material.bundle.journal.len() <= 128,
        "invalid current account recovery material",
    )?;
    let root = Identity::from_secret_hex(&material.root_secret_hex)
        .map_err(|_| AuthorizationError::InvalidProof("invalid recovery root material"))?;
    let recovery = Identity::from_secret_hex(&material.recovery_secret_hex)
        .map_err(|_| AuthorizationError::InvalidProof("invalid recovery key material"))?;
    let state = replay_policies(&material.pins, &material.bundle.journal)?;
    let account = state
        .account(&material.scope.account_id)
        .ok_or(AuthorizationError::InvalidProof("missing recovery account"))?;
    require(
        account.current_root == PublicKey::of(&root)
            && account.recovery.threshold == 1
            && account.recovery.keys == [PublicKey::of(&recovery)]
            && material.scope.space_id == SpaceId::personal(&material.scope.account_id),
        "recovery material policy mismatch",
    )?;
    let certificate = state.find_device(&material.scope.device_id)?;
    let auth = verify_client_authorization(
        &serde_json::to_string(&material.bundle)
            .map_err(|_| AuthorizationError::InvalidProof("invalid recovery bundle"))?,
        &serde_json::to_string(&material.pins)
            .map_err(|_| AuthorizationError::InvalidProof("invalid recovery pins"))?,
        material.scope.account_id.as_str(),
        material.scope.space_id.as_str(),
        certificate.payload.public_key.as_str(),
        certificate.payload.not_before_ms as f64,
    )
    .map_err(|_| AuthorizationError::InvalidProof("recovery backup authorization mismatch"))?;
    require(
        auth.scope() == &material.scope
            && auth.audience_public_key() == &material.entry.node_public_key,
        "recovery entry pin mismatch",
    )?;
    Ok(recovery)
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen(js_name = inspectAccountRecoveryMaterial))]
pub fn inspect_account_recovery_material(
    material_json: &str,
    now_ms: f64,
) -> crate::Result<String> {
    let inspect = || -> Result<serde_json::Value> {
        timestamp(now_ms)?;
        let material = read_material(material_json)?;
        validate_material(&material)?;
        Ok(
            serde_json::json!({"scope": &material.scope, "pins": &material.pins, "entry": &material.entry}),
        )
    };
    encode(&inspect().map_err(|e| e.to_string())?)
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen(js_name = buildAccountRecoveryMaterial))]
pub fn build_account_recovery_material(
    created: &CreatedAccount,
    root: &Identity,
    recovery: &Identity,
    endpoint: &str,
    now_ms: f64,
) -> crate::Result<String> {
    let build = || -> Result<RecoveryMaterial> {
        timestamp(now_ms)?;
        let auth = verify_client_authorization(
            &created
                .bundle_json()
                .map_err(|_| AuthorizationError::InvalidProof("invalid created bundle"))?,
            &created
                .pins_json()
                .map_err(|_| AuthorizationError::InvalidProof("invalid created pins"))?,
            created.authorization.scope.account_id.as_str(),
            created.authorization.scope.space_id.as_str(),
            &replay_policies(&created.pins, &created.bundle.journal)?
                .find_device(&created.authorization.scope.device_id)?
                .payload
                .public_key
                .to_string(),
            now_ms,
        )
        .map_err(|_| AuthorizationError::InvalidProof("created account authorization expired"))?;
        let material = RecoveryMaterial {
            format: "vibeface-account-recovery".to_owned(),
            version: 2,
            root_secret_hex: root.secret_hex(),
            recovery_secret_hex: recovery.secret_hex(),
            scope: auth.scope().clone(),
            pins: created.pins.clone(),
            bundle: created.bundle.clone(),
            entry: RecoveryEntry {
                endpoint: endpoint.to_owned(),
                node_public_key: auth.audience_public_key().clone(),
            },
        };
        validate_material(&material)?;
        Ok(material)
    };
    let mut encoded = encode(&build().map_err(|e| e.to_string())?)?;
    if encoded.len() > MAX_RECOVERY_MATERIAL_BYTES {
        encoded.zeroize();
        return Err("account recovery material requires checkpoint".to_owned());
    }
    Ok(encoded)
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen(js_name = buildRecoveredAccountRecoveryMaterial))]
pub fn build_recovered_account_recovery_material(
    created: &CreatedAccount,
    next_root: &Identity,
    previous_material_json: &str,
    endpoint: &str,
    now_ms: f64,
) -> crate::Result<String> {
    let material = read_material(previous_material_json).map_err(|e| e.to_string())?;
    let recovery = validate_material(&material).map_err(|e| e.to_string())?;
    require(
        created.authorization.scope.account_id == material.scope.account_id
            && created.authorization.scope.space_id == material.scope.space_id,
        "recovered material belongs to another account",
    )
    .map_err(|e| e.to_string())?;
    build_account_recovery_material(created, next_root, &recovery, endpoint, now_ms)
}

fn revoke_devices(
    state: &VerifiedPolicySet,
    registered: &[ObjectHash],
    revoked: &mut Vec<DeviceId>,
    keep_service: Option<&ServiceId>,
) -> Result<Vec<ObjectHash>> {
    let mut retained = vec![];
    for hash in registered {
        let certificate = state.device(hash).ok_or(AuthorizationError::InvalidProof(
            "recovery requires complete device inventory",
        ))?;
        if keep_service
            .is_some_and(|service| certificate.payload.subject == Actor::Service(service.clone()))
        {
            retained.push(hash.clone());
        } else {
            revoked.push(certificate.payload.device.clone());
        }
    }
    revoked.sort();
    revoked.dedup();
    Ok(retained)
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen(js_name = prepareAccountRecovery))]
pub fn prepare_account_recovery(
    material_json: &str,
    latest_bundle_json: &str,
    next_root: &Identity,
    next_client: &Identity,
    claim: &VerifiedNodeClaim,
    certificate_expires_at_ms: f64,
    now_ms: f64,
) -> crate::Result<PreparedAccountRecovery> {
    let prepare = || -> Result<PreparedAccountRecovery> {
        let material = read_material(material_json)?;
        let recovery = validate_material(&material)?;
        let now_ms = timestamp(now_ms)?;
        let expires_at_ms = timestamp(certificate_expires_at_ms)?;
        validate_window(now_ms, expires_at_ms, now_ms)?;
        let node_key = claim.claim.payload.node_public_key.clone();
        validate_claim(
            &claim.claim,
            &node_key,
            &claim.claim.payload.claim_nonce,
            now_ms,
        )?;
        let latest = read_bundle(latest_bundle_json)?;
        require(
            latest.journal.len() <= 118,
            "recovery journal requires checkpoint",
        )?;
        let mut state = replay_policies(&material.pins, &latest.journal)?;
        let account = state
            .account(&material.scope.account_id)
            .ok_or(AuthorizationError::InvalidProof(
                "missing current recovery account",
            ))?
            .clone();
        let space = state
            .space(&material.scope.space_id)
            .ok_or(AuthorizationError::InvalidProof(
                "missing current personal space",
            ))?
            .clone();
        require(
            space.root
                == SpaceRoot::Personal {
                    owner: material.scope.account_id.clone(),
                    genesis_root: account.genesis_root.clone(),
                }
                && account.recovery.threshold == 1
                && account.recovery.keys == [PublicKey::of(&recovery)],
            "recovery key is no longer current",
        )?;
        let root_key = PublicKey::of(next_root);
        let client_key = PublicKey::of(next_client);
        require(
            BTreeSet::from([
                root_key.clone(),
                client_key.clone(),
                node_key.clone(),
                PublicKey::of(&recovery),
            ])
            .len()
                == 4
                && state
                    .account_history(&account.account)
                    .is_some_and(|history| {
                        history.iter().all(|a| {
                            a.payload.current_root != root_key
                                && a.payload.current_root != client_key
                        })
                    })
                && !state.devices.values().any(|c| {
                    c.payload.public_key == client_key || c.payload.public_key == root_key
                }),
            "recovery requires independent new root and client keys",
        )?;
        let expected_account_head = state
            .head(&PolicyDomain::Account(account.account.clone()))
            .cloned()
            .ok_or(AuthorizationError::PolicyConflict)?;
        let expected_space_head = state
            .head(&PolicyDomain::Space(space.space.clone()))
            .cloned()
            .ok_or(AuthorizationError::PolicyConflict)?;
        let authority = AuthorityBinding::from_key(node_key.clone());
        let mut recovered = account.clone();
        recovered.previous = Some(expected_account_head.clone());
        recovered.authority_epoch = account
            .authority_epoch
            .checked_add(1)
            .ok_or(AuthorizationError::PolicyConflict)?;
        recovered.sequence = 0;
        recovered.current_root = root_key.clone();
        recovered.root_generation = account
            .root_generation
            .checked_add(1)
            .ok_or(AuthorizationError::PolicyConflict)?;
        recovered.authority = authority.clone();
        recovered.devices = revoke_devices(
            &state,
            &account.devices,
            &mut recovered.revoked_devices,
            Some(&authority.service),
        )?;
        recovered.enrollment.clear();
        recovered.authenticators.clear();
        recovered.root_acceptance = None;
        recovered.recovery_approvals.clear();
        recovered.approve_recovery(&recovery)?;
        recovered.accept_root(next_root)?;
        let account_recovery = Signed::sign(recovered, &recovery)?;
        let recovered_head = state.install_account(account_recovery.clone())?;
        let certificate = |key: PublicKey, subject, uses, nonce| DeviceCertificate {
            version: AUTH_VERSION,
            owner: PolicyDomain::Account(account.account.clone()),
            subject,
            device: DeviceId::from_key(&key),
            public_key: key,
            uses,
            possession: None,
            enrollment_grant: None,
            issued_under: recovered_head.clone(),
            nonce,
            not_before_ms: now_ms,
            expires_at_ms,
        };
        let mut client_certificate = certificate(
            client_key,
            Actor::Account(account.account.clone()),
            vec![
                DeviceUse::Noise,
                DeviceUse::AccountInvocation,
                DeviceUse::EnrollDevice,
            ],
            Nonce::random()?,
        );
        client_certificate.prove_possession(next_client)?;
        let client_certificate = Signed::sign(client_certificate, next_root)?;
        state.install_device(client_certificate.clone(), now_ms)?;
        let node_certificate = certificate(
            node_key,
            Actor::Service(authority.service.clone()),
            vec![DeviceUse::Noise, DeviceUse::ServiceInvocation],
            claim.claim.payload.claim_nonce.clone(),
        );
        let mut space_document = space.clone();
        space_document.previous = Some(expected_space_head.clone());
        space_document.authority_epoch = space
            .authority_epoch
            .checked_add(1)
            .ok_or(AuthorizationError::PolicyConflict)?;
        space_document.sequence = 0;
        space_document.current_root = root_key;
        space_document.root_generation = account_recovery.payload.root_generation;
        space_document.authority = authority;
        space_document.recovery = account_recovery.payload.recovery.clone();
        space_document.devices = revoke_devices(
            &state,
            &space.devices,
            &mut space_document.revoked_devices,
            None,
        )?;
        space_document.grants.clear();
        space_document.memberships.clear();
        space_document.governance_grant = None;
        space_document.root_acceptance = None;
        space_document.recovery_approvals.clear();
        let has_project = space
            .resources
            .iter()
            .filter_map(|hash| state.resources.get(hash))
            .any(|record| {
                record.payload.kind == ResourceKind::Project
                    && record.payload.host == space_document.authority.service
            });
        let new_project = if has_project {
            None
        } else {
            Some((ResourceId::random()?, Nonce::random()?))
        };
        let selected_project = new_project
            .as_ref()
            .map(|(id, _)| id.clone())
            .or_else(|| {
                space
                    .resources
                    .iter()
                    .filter_map(|h| state.resources.get(h))
                    .find(|r| {
                        r.payload.kind == ResourceKind::Project
                            && r.payload.host == space_document.authority.service
                    })
                    .map(|r| r.payload.resource.clone())
            })
            .ok_or(AuthorizationError::InvalidProof("missing recovery project"))?;
        let recovery_space_head = space_stamp(&Signed::sign(space_document.clone(), next_root)?)?;
        let owner_grant = Signed::sign(
            initial_owner_grant(
                &account.account,
                &space.space,
                &Audience {
                    service: space_document.authority.service.clone(),
                    device: space_document.authority.device.clone(),
                },
                &recovery_space_head,
                now_ms,
                expires_at_ms,
                Nonce::random()?,
            ),
            next_root,
        )?;
        let service_grant = initial_service_grant(&owner_grant, &selected_project, next_client)?;
        Ok(PreparedAccountRecovery {
            request: RecoveryNodeRequest {
                claim: claim.claim.clone(),
                expected_account_head,
                expected_space_head,
                account_recovery,
                node_certificate,
            },
            pins: material.pins.clone(),
            journal: latest.journal,
            client_certificate,
            space_document,
            owner_grant,
            service_grant,
            new_project,
        })
    };
    prepare().map_err(|e| e.to_string())
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen)]
impl PreparedAccountRecovery {
    #[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen(js_name = nodeRequestJson))]
    pub fn node_request_json(&self) -> crate::Result<String> {
        encode(&self.request)
    }

    #[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen(js_name = commitJson))]
    pub fn commit_json(&self, recovered: &CreatedAccount) -> crate::Result<String> {
        require(
            recovered.bundle.journal.starts_with(&self.journal)
                && recovered.bundle.journal.get(self.journal.len())
                    == Some(&JournalEntry::Account(
                        self.request.account_recovery.clone(),
                    ))
                && recovered.authorization.scope.account_id
                    == self.request.account_recovery.payload.account
                && recovered.authorization.scope.device_id
                    == self.client_certificate.payload.device,
            "recovery result belongs to another proposal",
        )
        .map_err(|e| e.to_string())?;
        encode(&RecoveryCommit {
            expected_account_head: &self.request.expected_account_head,
            expected_space_head: &self.request.expected_space_head,
            journal_delta: &recovered.bundle.journal[self.journal.len()..],
            claim_nonce: &self.request.claim.payload.claim_nonce,
            request_hash: ObjectHash::body(&self.request).map_err(|e| e.to_string())?,
        })
    }
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen(js_name = signRecoveryNodePossession))]
pub fn sign_recovery_node_possession(
    request_json: &str,
    node: &Identity,
    trusted_pins_json: &str,
    trusted_bundle_json: &str,
    expected_claim_nonce: &str,
    now_ms: f64,
) -> crate::Result<String> {
    let sign = || -> Result<Signature> {
        require(
            request_json.len() <= MAX_PROOF_BYTES && trusted_pins_json.len() <= MAX_PROOF_BYTES,
            "recovery possession request too large",
        )?;
        let request: RecoveryNodeRequest = serde_json::from_str(request_json)
            .map_err(|_| AuthorizationError::InvalidProof("invalid recovery possession request"))?;
        let pins: Vec<PolicyPin> = serde_json::from_str(trusted_pins_json)
            .map_err(|_| AuthorizationError::InvalidProof("invalid trusted recovery pins"))?;
        let bundle = read_bundle(trusted_bundle_json)?;
        let mut state = replay_policies(&pins, &bundle.journal)?;
        let now_ms = timestamp(now_ms)?;
        let node_key = PublicKey::of(node);
        let nonce = Nonce::parse(expected_claim_nonce)?;
        validate_claim(&request.claim, &node_key, &nonce, now_ms)?;
        let a = &request.account_recovery.payload;
        require(
            state.head(&PolicyDomain::Account(a.account.clone()))
                == Some(&request.expected_account_head)
                && state.head(&PolicyDomain::Space(SpaceId::personal(&a.account)))
                    == Some(&request.expected_space_head),
            "recovery proposal has stale policy heads",
        )?;
        let previous = state
            .account(&a.account)
            .ok_or(AuthorizationError::PolicyConflict)?;
        require(
            request.account_recovery.issuer != previous.current_root
                && a.current_root != previous.current_root
                && a.authority_epoch
                    == previous
                        .authority_epoch
                        .checked_add(1)
                        .ok_or(AuthorizationError::PolicyConflict)?
                && a.sequence == 0
                && a.authority == AuthorityBinding::from_key(node_key.clone())
                && !a.recovery.keys.contains(&node_key),
            "node possession requires an independent recovery transition",
        )?;
        let head = state.install_account(request.account_recovery.clone())?;
        let c = &request.node_certificate;
        require(
            c.version == AUTH_VERSION
                && c.owner == PolicyDomain::Account(a.account.clone())
                && c.subject == Actor::Service(ServiceId::from_key(&node_key))
                && c.public_key == node_key
                && c.device == DeviceId::from_key(&node_key)
                && c.uses == [DeviceUse::Noise, DeviceUse::ServiceInvocation]
                && c.possession.is_none()
                && c.enrollment_grant.is_none()
                && c.issued_under == head
                && c.nonce == nonce
                && c.not_before_ms >= request.claim.payload.issued_at_ms
                && c.not_before_ms < request.claim.payload.expires_at_ms,
            "recovery node certificate changed enrollment scope",
        )?;
        validate_window(c.not_before_ms, c.expires_at_ms, now_ms)?;
        Signature::parse(
            &node
                .sign("device-possession-v2", &c.possession_bytes()?)
                .map_err(|_| AuthorizationError::InvalidProof("recovery node signing failed"))?,
        )
    };
    sign().map(|s| s.to_string()).map_err(|e| e.to_string())
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen(js_name = finishAccountRecovery))]
pub fn finish_account_recovery(
    prepared: &PreparedAccountRecovery,
    next_root: &Identity,
    node_possession_signature: &str,
    now_ms: f64,
) -> crate::Result<CreatedAccount> {
    let finish = || -> Result<(ClientAuthorizationBundle, Vec<PolicyPin>)> {
        let now_ms = timestamp(now_ms)?;
        require(
            PublicKey::of(next_root) == prepared.request.account_recovery.payload.current_root,
            "recovery root changed",
        )?;
        validate_claim(
            &prepared.request.claim,
            &prepared.request.node_certificate.public_key,
            &prepared.request.node_certificate.nonce,
            now_ms,
        )?;
        let mut state = replay_policies(&prepared.pins, &prepared.journal)?;
        let account_head = state.install_account(prepared.request.account_recovery.clone())?;
        let client_hash = state.install_device(prepared.client_certificate.clone(), now_ms)?;
        let mut node_certificate = prepared.request.node_certificate.clone();
        node_certificate.possession = Some(Signature::parse(node_possession_signature)?);
        let node_certificate = Signed::sign(node_certificate, next_root)?;
        let node_hash = state.install_device(node_certificate.clone(), now_ms)?;
        let mut account = prepared.request.account_recovery.payload.clone();
        account.previous = Some(account_head);
        account.sequence = 1;
        account.devices = vec![client_hash, node_hash];
        account.devices.sort();
        account.enrollment = vec![EnrollmentAuthority {
            device: prepared.client_certificate.payload.device.clone(),
            rights: vec![
                AccountRight::EnrollDevice,
                AccountRight::RevokeDevice,
                AccountRight::ManageAuthenticators,
            ],
            expires_at_ms: prepared.client_certificate.payload.expires_at_ms,
        }];
        account.root_acceptance = None;
        account.recovery_approvals.clear();
        let account = Signed::sign(account, next_root)?;
        let account_head = state.install_account(account.clone())?;
        let space_recovery = Signed::sign(prepared.space_document.clone(), next_root)?;
        let space_head = state.install_space(space_recovery.clone())?;
        let space = prepared.space_document.space.clone();
        let service = ServiceId::from_key(&prepared.request.node_certificate.public_key);
        let new_resource = if let Some((resource, nonce)) = &prepared.new_project {
            Some(Signed::sign(
                ResourceRecord {
                    version: AUTH_VERSION,
                    space: space.clone(),
                    resource: resource.clone(),
                    kind: ResourceKind::Project,
                    host: service,
                    issued_under: space_head.clone(),
                    nonce: nonce.clone(),
                },
                next_root,
            )?)
        } else {
            None
        };
        let grant = prepared.owner_grant.clone();
        let grant_leaf = state.install_grant(grant.clone(), now_ms)?;
        let mut space_document = prepared.space_document.clone();
        space_document.previous = Some(space_head);
        space_document.sequence = 1;
        space_document.grants = vec![grant_leaf.clone()];
        if let Some(record) = &new_resource {
            space_document
                .resources
                .push(state.install_resource(record.clone())?);
            space_document.resources.sort();
        }
        let space_document = Signed::sign(space_document, next_root)?;
        let space_head = state.install_space(space_document.clone())?;
        let service_hash = state.install_grant(prepared.service_grant.clone(), now_ms)?;
        let mut service_document = space_document.payload.clone();
        service_document.previous = Some(space_head);
        service_document.sequence += 1;
        service_document.grants.push(service_hash);
        service_document.grants.sort();
        let service_document = Signed::sign(service_document, next_root)?;
        let space_head = state.install_space(service_document.clone())?;
        let mut pins = prepared.pins.clone();
        for pin in &mut pins {
            if pin.domain == account_head.domain {
                pin.minimum_head = Some(account_head.clone());
            } else if pin.domain == space_head.domain {
                pin.minimum_head = Some(space_head.clone());
            }
        }
        let mut journal = prepared.journal.clone();
        journal.extend([
            JournalEntry::Account(prepared.request.account_recovery.clone()),
            JournalEntry::Device(prepared.client_certificate.clone()),
            JournalEntry::Device(node_certificate),
            JournalEntry::Account(account),
            JournalEntry::Space(space_recovery),
        ]);
        if let Some(record) = new_resource {
            journal.push(JournalEntry::Resource(record));
        }
        journal.extend([
            JournalEntry::Grant(grant),
            JournalEntry::Space(space_document),
            JournalEntry::Grant(prepared.service_grant.clone()),
            JournalEntry::Space(service_document),
        ]);
        Ok((
            ClientAuthorizationBundle {
                journal,
                grant_leaf,
            },
            pins,
        ))
    };
    let (bundle, pins) = finish().map_err(|e| e.to_string())?;
    let authorization = verify_client_authorization(
        &encode(&bundle)?,
        &encode(&pins)?,
        prepared.request.account_recovery.payload.account.as_str(),
        prepared.space_document.space.as_str(),
        prepared.client_certificate.payload.public_key.as_str(),
        now_ms,
    )?;
    Ok(CreatedAccount {
        bundle,
        pins,
        authorization,
    })
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

    fn nonce(byte: u8) -> String {
        Nonce::from_bytes([byte; 32]).to_string()
    }

    fn claim(node: u8, nonce_byte: u8, now: f64) -> VerifiedNodeClaim {
        verify_node_claim(
            &create_node_claim(&key(node), &nonce(nonce_byte), now, now + 100_000.0).unwrap(),
            &key(node).public_key_hex(),
            &nonce(nonce_byte),
            now,
        )
        .unwrap()
    }

    fn account() -> CreatedAccount {
        let p = prepare_account_creation(&key(1), &key(2), &key(3), &claim(4, 9, NOW), END, NOW)
            .unwrap();
        let proof =
            sign_node_possession(&p.node_request_json().unwrap(), &key(4), &nonce(9), NOW).unwrap();
        finish_account_creation(&p, &key(1), &proof, NOW).unwrap()
    }

    fn material(account: &CreatedAccount) -> String {
        build_account_recovery_material(account, &key(1), &key(2), ENDPOINT, NOW).unwrap()
    }

    fn prepared(account: &CreatedAccount, node: u8) -> PreparedAccountRecovery {
        prepare_account_recovery(
            &material(account),
            &account.bundle_json().unwrap(),
            &key(5),
            &key(6),
            &claim(node, 8, RESTORE),
            END,
            RESTORE,
        )
        .unwrap()
    }

    fn proof(p: &PreparedAccountRecovery, account: &CreatedAccount, node: u8) -> String {
        sign_recovery_node_possession(
            &p.node_request_json().unwrap(),
            &key(node),
            &account.pins_json().unwrap(),
            &account.bundle_json().unwrap(),
            &nonce(8),
            RESTORE,
        )
        .unwrap()
    }

    #[test]
    fn recovery_preserves_account_and_resources_and_revokes_old_clients_and_grants() {
        let original = account();
        let p = prepared(&original, 4);
        let recovered =
            finish_account_recovery(&p, &key(5), &proof(&p, &original, 4), RESTORE).unwrap();
        let retried =
            finish_account_recovery(&p, &key(5), &proof(&p, &original, 4), RESTORE + 1.0).unwrap();
        assert_eq!(
            recovered.bundle_json().unwrap(),
            retried.bundle_json().unwrap()
        );
        let scope = recovered.authorization.scope();
        assert_eq!(scope.account_id, original.authorization.scope.account_id);
        assert_eq!(scope.space_id, original.authorization.scope.space_id);
        assert_eq!(scope.device_id, DeviceId::from_key(&PublicKey::of(&key(6))));
        let state = replay_policies(&recovered.pins, &recovered.bundle.journal).unwrap();
        let current = state.account(&scope.account_id).unwrap();
        assert_eq!(current.current_root, PublicKey::of(&key(5)));
        assert_eq!(
            (
                current.authority_epoch,
                current.root_generation,
                current.sequence
            ),
            (1, 1, 1)
        );
        assert!(current.authenticators.is_empty());
        assert!(
            current
                .revoked_devices
                .contains(&DeviceId::from_key(&PublicKey::of(&key(3))))
        );
        assert!(
            !current
                .revoked_devices
                .contains(&DeviceId::from_key(&PublicKey::of(&key(4))))
        );
        let space = state.space(&scope.space_id).unwrap();
        assert_eq!(
            (space.authority_epoch, space.root_generation, space.sequence),
            (1, 1, 2)
        );
        assert_eq!(space.current_root, PublicKey::of(&key(5)));
        assert_eq!(space.resources, p.space_document.resources);
        assert!(!space.grants.contains(&original.bundle.grant_leaf));
        let service: Signed<Grant> =
            serde_json::from_str(&recovered.service_grant_json().unwrap()).unwrap();
        assert_eq!(service.issuer, PublicKey::of(&key(6)));
        assert_eq!(service.payload.remaining_delegations, 0);
        assert_eq!(
            service.payload.parent.as_ref(),
            Some(&recovered.bundle.grant_leaf)
        );
        assert!(space.grants.contains(&recovered.bundle.grant_leaf));
        assert!(space.grants.contains(&service.hash().unwrap()));
        assert_eq!(space.grants.len(), 2);
        let old_service: Signed<Grant> =
            serde_json::from_str(&original.service_grant_json().unwrap()).unwrap();
        assert!(!space.grants.contains(&old_service.hash().unwrap()));
        assert_eq!(
            recovered.authorization.account_enrollment_rights(),
            [
                AccountRight::EnrollDevice,
                AccountRight::RevokeDevice,
                AccountRight::ManageAuthenticators
            ]
        );
        assert!(
            verify_client_authorization(
                &recovered.bundle_json().unwrap(),
                &recovered.pins_json().unwrap(),
                scope.account_id.as_str(),
                scope.space_id.as_str(),
                &key(3).public_key_hex(),
                RESTORE,
            )
            .is_err()
        );
        let commit: serde_json::Value =
            serde_json::from_str(&p.commit_json(&recovered).unwrap()).unwrap();
        assert_eq!(commit["journal_delta"].as_array().unwrap().len(), 9);
        assert_eq!(commit["claim_nonce"], nonce(8));
        assert_eq!(
            commit["request_hash"],
            ObjectHash::body(&p.request).unwrap().to_string()
        );
        assert!(p.commit_json(&original).is_err());
        assert!(
            build_account_recovery_material(&recovered, &key(5), &key(2), ENDPOINT, RESTORE)
                .is_ok()
        );
        let updated = build_recovered_account_recovery_material(
            &recovered,
            &key(5),
            &material(&original),
            ENDPOINT,
            RESTORE,
        )
        .unwrap();
        let updated: RecoveryMaterial = serde_json::from_str(&updated).unwrap();
        assert_eq!(updated.scope, *recovered.authorization.scope());
        assert!(
            build_recovered_account_recovery_material(&recovered, &key(5), "{}", ENDPOINT, RESTORE)
                .is_err()
        );
        let request = p.node_request_json().unwrap();
        assert!(!request.contains(&key(1).secret_hex()));
        assert!(!request.contains(&key(2).secret_hex()));
    }

    #[test]
    fn replacement_node_changes_authority_without_rehosting_original_data() {
        let original = account();
        let p = prepared(&original, 7);
        let recovered =
            finish_account_recovery(&p, &key(5), &proof(&p, &original, 7), RESTORE).unwrap();
        let retried =
            finish_account_recovery(&p, &key(5), &proof(&p, &original, 7), RESTORE + 1.0).unwrap();
        assert_eq!(
            recovered.bundle_json().unwrap(),
            retried.bundle_json().unwrap()
        );
        let state = replay_policies(&recovered.pins, &recovered.bundle.journal).unwrap();
        let current = state
            .account(&recovered.authorization.scope.account_id)
            .unwrap();
        assert_eq!(current.authority.public_key, PublicKey::of(&key(7)));
        assert!(
            current
                .revoked_devices
                .contains(&DeviceId::from_key(&PublicKey::of(&key(4))))
        );
        let space = state
            .space(&recovered.authorization.scope.space_id)
            .unwrap();
        assert_eq!(space.resources.len(), p.space_document.resources.len() + 1);
        let original_hash = &p.space_document.resources[0];
        assert!(space.resources.contains(original_hash));
        let resource = state.resources.get(original_hash).unwrap();
        assert_eq!(
            resource.payload.host,
            ServiceId::from_key(&PublicKey::of(&key(4)))
        );
        let new_hash = space
            .resources
            .iter()
            .find(|hash| !p.space_document.resources.contains(hash))
            .unwrap();
        let new_resource = state.resources.get(new_hash).unwrap();
        assert_eq!(new_resource.payload.kind, ResourceKind::Project);
        assert_eq!(
            new_resource.payload.host,
            ServiceId::from_key(&PublicKey::of(&key(7)))
        );
        assert_ne!(new_resource.payload.resource, resource.payload.resource);
        let commit: serde_json::Value =
            serde_json::from_str(&p.commit_json(&recovered).unwrap()).unwrap();
        assert_eq!(commit["journal_delta"].as_array().unwrap().len(), 10);
    }

    #[test]
    fn material_rejects_wrong_roles_entry_pins_and_legacy_payloads() {
        let original = account();
        assert!(
            build_account_recovery_material(&original, &key(7), &key(2), ENDPOINT, NOW).is_err()
        );
        assert!(
            build_account_recovery_material(&original, &key(1), &key(7), ENDPOINT, NOW).is_err()
        );
        assert!(
            build_account_recovery_material(
                &original,
                &key(1),
                &key(2),
                "javascript:alert(1)",
                NOW
            )
            .is_err()
        );
        let original_material = material(&original);
        for mutation in 0..6 {
            let mut m: serde_json::Value = serde_json::from_str(&original_material).unwrap();
            match mutation {
                0 => m["version"] = 1.into(),
                1 => m["root_secret_hex"] = key(7).secret_hex().into(),
                2 => m["entry"]["node_public_key"] = key(7).public_key_hex().into(),
                3 => {
                    m["scope"]["account_id"] = AccountId::from_root(&PublicKey::of(&key(7)))
                        .to_string()
                        .into()
                }
                4 => m["pins"][0]["genesis_root"] = key(7).public_key_hex().into(),
                5 => m["recovery_secret_hex"] = key(7).secret_hex().into(),
                _ => unreachable!(),
            }
            assert!(inspect_account_recovery_material(&m.to_string(), RESTORE).is_err());
            assert!(
                prepare_account_recovery(
                    &m.to_string(),
                    &original.bundle_json().unwrap(),
                    &key(5),
                    &key(6),
                    &claim(4, 8, RESTORE),
                    END,
                    RESTORE,
                )
                .is_err()
            );
        }
        assert!(
            prepare_account_recovery(
                "{}",
                &original.bundle_json().unwrap(),
                &key(5),
                &key(6),
                &claim(4, 8, RESTORE),
                END,
                RESTORE
            )
            .is_err()
        );
    }

    #[test]
    fn inspection_exposes_only_verified_backup_context_after_device_expiry() {
        let original = account();
        let public = inspect_account_recovery_material(&material(&original), END + 1.0).unwrap();
        assert!(!public.contains("secret_hex"));
        assert!(!public.contains(&key(1).secret_hex()));
        assert!(!public.contains(&key(2).secret_hex()));
        let value: serde_json::Value = serde_json::from_str(&public).unwrap();
        assert_eq!(value.as_object().unwrap().len(), 3);
        assert_eq!(
            value["scope"],
            serde_json::to_value(original.authorization.scope()).unwrap()
        );
        assert_eq!(value["entry"]["endpoint"], ENDPOINT);
        assert_eq!(value["entry"]["node_public_key"], key(4).public_key_hex());
        assert!(inspect_account_recovery_material(&material(&original), f64::NAN).is_err());
    }

    #[test]
    fn recovery_signer_requires_current_trusted_heads_and_exact_service_scope() {
        let original = account();
        let p = prepared(&original, 4);
        let request = p.node_request_json().unwrap();
        assert!(
            sign_recovery_node_possession(
                &request,
                &key(7),
                &original.pins_json().unwrap(),
                &original.bundle_json().unwrap(),
                &nonce(8),
                RESTORE
            )
            .is_err()
        );
        assert!(
            sign_recovery_node_possession(
                &request,
                &key(4),
                &original.pins_json().unwrap(),
                &original.bundle_json().unwrap(),
                &nonce(7),
                RESTORE
            )
            .is_err()
        );
        for mutation in 0..4 {
            let mut r = p.request.clone();
            match mutation {
                0 => r.expected_space_head.sequence += 1,
                1 => r.account_recovery.payload.current_root = PublicKey::of(&key(7)),
                2 => r.node_certificate.uses.push(DeviceUse::EnrollDevice),
                3 => {
                    r.node_certificate.subject =
                        Actor::Account(r.account_recovery.payload.account.clone())
                }
                _ => unreachable!(),
            }
            assert!(
                sign_recovery_node_possession(
                    &encode(&r).unwrap(),
                    &key(4),
                    &original.pins_json().unwrap(),
                    &original.bundle_json().unwrap(),
                    &nonce(8),
                    RESTORE
                )
                .is_err()
            );
        }
        let recovered =
            finish_account_recovery(&p, &key(5), &proof(&p, &original, 4), RESTORE).unwrap();
        assert!(
            sign_recovery_node_possession(
                &request,
                &key(4),
                &recovered.pins_json().unwrap(),
                &recovered.bundle_json().unwrap(),
                &nonce(8),
                RESTORE
            )
            .is_err()
        );
    }

    #[test]
    fn changed_live_recovery_policy_rejects_an_old_backup() {
        let original = account();
        let mut state = replay_policies(&original.pins, &original.bundle.journal).unwrap();
        let account_id = &original.authorization.scope.account_id;
        let mut account = state.account(account_id).unwrap().clone();
        account.previous = state
            .head(&PolicyDomain::Account(account_id.clone()))
            .cloned();
        account.sequence += 1;
        account.recovery.generation += 1;
        account.recovery.keys = vec![PublicKey::of(&key(7))];
        account.approve_recovery(&key(2)).unwrap();
        let signed = Signed::sign(account, &key(1)).unwrap();
        state.install_account(signed.clone()).unwrap();
        let mut latest = original.bundle.clone();
        latest.journal.push(JournalEntry::Account(signed));
        assert!(
            prepare_account_recovery(
                &material(&original),
                &encode(&latest).unwrap(),
                &key(5),
                &key(6),
                &claim(4, 8, RESTORE),
                END,
                RESTORE
            )
            .is_err()
        );
        let mut rollback = original.bundle.clone();
        rollback.journal.pop();
        assert!(
            prepare_account_recovery(
                &material(&original),
                &encode(&rollback).unwrap(),
                &key(5),
                &key(6),
                &claim(4, 8, RESTORE),
                END,
                RESTORE
            )
            .is_err()
        );
    }

    #[test]
    fn finish_rejects_missing_wrong_or_tampered_possession_and_expired_claim() {
        let original = account();
        let mut p = prepared(&original, 4);
        let sig = proof(&p, &original, 4);
        assert!(finish_account_recovery(&p, &key(5), "", RESTORE).is_err());
        assert!(finish_account_recovery(&p, &key(7), &sig, RESTORE).is_err());
        assert!(finish_account_recovery(&p, &key(5), &sig, RESTORE + 100_000.0).is_err());
        let wrong = key(7)
            .sign(
                "device-possession-v2",
                &p.request.node_certificate.possession_bytes().unwrap(),
            )
            .unwrap();
        assert!(finish_account_recovery(&p, &key(5), &wrong, RESTORE).is_err());
        p.request.node_certificate.expires_at_ms -= 1;
        assert!(finish_account_recovery(&p, &key(5), &sig, RESTORE).is_err());
    }

    #[test]
    fn recovery_requires_new_and_independent_root_and_client_keys() {
        let original = account();
        for (root, client) in [(1, 6), (3, 6), (5, 3), (5, 1), (5, 5), (2, 6), (5, 4)] {
            assert!(
                prepare_account_recovery(
                    &material(&original),
                    &original.bundle_json().unwrap(),
                    &key(root),
                    &key(client),
                    &claim(4, 8, RESTORE),
                    END,
                    RESTORE
                )
                .is_err()
            );
        }
    }
}
