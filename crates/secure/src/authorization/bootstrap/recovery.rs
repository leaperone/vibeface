//! Account recovery preserves identity and replaces device authorization atomically.

mod checkpoint;
pub mod context;
pub mod cross_node;
pub mod node_enrollment;
pub mod registration;
pub mod stream;

use super::*;
pub use checkpoint::RecoveryPolicyCheckpoint;
pub(crate) use checkpoint::{
    compact_recovery_material, compact_recovery_material_in_state, material_state,
    recovery_material_usage,
};
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub policy_checkpoint: Option<RecoveryPolicyCheckpoint>,
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
pub struct RecoveryNodeRequest {
    pub claim: Signed<NodeClaim>,
    pub expected_account_head: PolicyStamp,
    pub expected_space_head: PolicyStamp,
    pub account_recovery: Signed<AccountDocument>,
    pub node_certificate: DeviceCertificate,
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
    let state = material_state(material)?;
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
    checkpoint::scope_budget(
        &state,
        material,
        certificate.payload.not_before_ms,
        &mut Budget::default(),
    )?;
    Ok(recovery)
}

pub(super) fn current_root_material_context(
    material_json: &str,
    pins_json: &str,
    bundle_json: &str,
) -> Result<(
    AccountId,
    Identity,
    VerifiedPolicySet,
    Vec<PolicyPin>,
    ClientAuthorizationBundle,
)> {
    require(
        pins_json.len() <= MAX_PROOF_BYTES,
        "registration pins too large",
    )?;
    let material = read_material(material_json)?;
    validate_material(&material)?;
    let root = Identity::from_secret_hex(&material.root_secret_hex)
        .map_err(|_| AuthorizationError::InvalidProof("invalid first registration root"))?;
    let pins: Vec<PolicyPin> = serde_json::from_str(pins_json)
        .map_err(|_| AuthorizationError::InvalidProof("invalid first registration pins"))?;
    let bundle = read_bundle(bundle_json)?;
    let state = replay_policies(&pins, &bundle.journal)?;
    for saved in &material.pins {
        require(
            pins.iter()
                .any(|pin| pin.domain == saved.domain && pin.genesis_root == saved.genesis_root)
                && saved
                    .minimum_head
                    .as_ref()
                    .is_none_or(|head| state.roots.contains_key(head)),
            "first registration changed saved recovery pins",
        )?;
    }
    let account = state
        .account(&material.scope.account_id)
        .ok_or(AuthorizationError::Revoked)?;
    require(
        account.current_root == PublicKey::of(&root)
            && account.authority.public_key == material.entry.node_public_key,
        "first registration changed current root or saved node",
    )?;
    Ok((material.scope.account_id.clone(), root, state, pins, bundle))
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
    let _scope = crate::signature_verification::ScopedSignatureVerification::new();
    let build = || -> Result<RecoveryMaterial> {
        let now = timestamp(now_ms)?;
        let state = &created.authorization.policy_snapshot;
        require(
            created.authorization.policy_journal == created.bundle.journal
                && created
                    .pins
                    .iter()
                    .all(|pin| state.head(&pin.domain) == pin.minimum_head.as_ref()),
            "created recovery material differs from its verified policy state",
        )?;
        let auth = verify_client_policy_state(
            state.clone(),
            &created.bundle,
            &created.pins,
            created.authorization.scope.account_id.clone(),
            created.authorization.scope.space_id.clone(),
            state
                .find_device(&created.authorization.scope.device_id)?
                .payload
                .public_key
                .clone(),
            now,
        )?;
        let mut material = RecoveryMaterial {
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
            policy_checkpoint: None,
        };
        compact_recovery_material_in_state(&mut material, state, now)?;
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
    prepare_account_recovery_preserving_source(
        material_json,
        latest_bundle_json,
        next_root,
        next_client,
        claim,
        certificate_expires_at_ms,
        now_ms,
        None,
    )
}

fn prepare_account_recovery_preserving_source(
    material_json: &str,
    latest_bundle_json: &str,
    next_root: &Identity,
    next_client: &Identity,
    claim: &VerifiedNodeClaim,
    certificate_expires_at_ms: f64,
    now_ms: f64,
    preserved_source: Option<&ServiceId>,
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
            preserved_source.or(Some(&authority.service)),
        )?;
        recovered.enrollment.clear();
        recovered.authenticators.clear();
        recovered.authenticator_approval = None;
        recovered.device_enrollment = None;
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
            preserved_source,
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
        account.devices.retain(|hash| {
            state.devices.get(hash).is_some_and(|certificate| {
                certificate.payload.device != prepared.client_certificate.payload.device
                    && certificate.payload.device != node_certificate.payload.device
            })
        });
        account.devices.extend([client_hash, node_hash]);
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

    pub(super) fn account_with_enrolled_nodes(count: u8) -> CreatedAccount {
        enroll_nodes(account(), count)
    }

    fn enroll_nodes(mut created: CreatedAccount, count: u8) -> CreatedAccount {
        use node_enrollment::*;
        let backup = material(&created);
        for index in 0..count {
            let pins = created.pins_json().unwrap();
            let bundle = created.bundle_json().unwrap();
            let node = 20 + index;
            let claim_nonce = 100 + index;
            let prepared = prepare_account_node_enrollment(
                &backup,
                &pins,
                &bundle,
                &key(3),
                &claim(node, claim_nonce, NOW),
                END,
                NOW,
            )
            .unwrap();
            let possession = sign_account_node_enrollment_possession(
                &prepared.request_json().unwrap(),
                &key(node),
                &nonce(claim_nonce),
                NOW,
            )
            .unwrap();
            let prepared =
                prepare_account_node_enrollment_commit(&prepared, &backup, &possession, NOW)
                    .unwrap();
            let delta = commit_account_node_enrollment(
                &prepared.request_json().unwrap(),
                &key(4),
                &pins,
                &bundle,
                NOW,
            )
            .unwrap();
            let installed = finish_account_node_enrollment(&prepared, &delta, NOW).unwrap();
            created.pins = installed.pins().to_vec();
            created.bundle = installed.bundle().clone();
        }
        let required = created
            .pins
            .iter()
            .filter_map(|pin| pin.minimum_head.clone())
            .collect();
        let mut import = crate::authorization::policy_import::BoundedPolicyImport::new(
            created.pins.clone(),
            required,
        )
        .unwrap();
        for entry in &created.bundle.journal {
            let _scope = crate::signature_verification::ScopedSignatureVerification::new();
            import.apply_entry(entry.clone()).unwrap();
        }
        let heads = created
            .pins
            .iter()
            .filter_map(|pin| pin.minimum_head.clone())
            .collect::<Vec<_>>();
        let snapshot = import.finish(&heads).unwrap();
        created.authorization = verify_client_policy_state(
            snapshot.state().clone(),
            &created.bundle,
            &created.pins,
            created.authorization.scope.account_id.clone(),
            created.authorization.scope.space_id.clone(),
            PublicKey::of(&key(3)),
            NOW as u64,
        )
        .unwrap();
        created
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

    #[test]
    fn first_authenticator_material_stays_private_and_receipt_advances_the_pin() {
        use crate::authorization::enrollment::AuthenticatorEnrollmentIntent;
        use crate::webauthn::*;
        use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
        use coset::cbor::Value;
        use registration::*;
        let created = account();
        let backup = material(&created);
        let pins = created.pins_json().unwrap();
        let bundle = created.bundle_json().unwrap();
        let signed_intent =
            build_first_authenticator_enrollment_intent(&backup, &pins, &bundle, &key(3), NOW, END)
                .unwrap();
        let intent: Signed<AuthenticatorEnrollmentIntent> =
            serde_json::from_str(&signed_intent).unwrap();
        assert!(
            build_first_authenticator_enrollment_intent(&backup, &pins, &bundle, &key(4), NOW, END)
                .is_err()
        );
        assert!(
            build_first_authenticator_enrollment_intent(&backup, "[]", &bundle, &key(3), NOW, END)
                .is_err()
        );
        let pending = PendingPasskey::prepare(
            EnrollmentEnvelope {
                version: AUTH_VERSION,
                purpose: PasskeyPurpose::RegisterAuthenticator,
                account: intent.payload.account.clone(),
                device_public_key: intent.payload.requesting_device_public_key.clone(),
                rights: vec![AccountRight::ManageAuthenticators],
                target: intent.payload.target.clone(),
                enrollment_hash: ObjectHash::body(&intent.payload).unwrap(),
                expected_heads: intent.payload.expected_heads.clone(),
                nonce: Nonce::from_bytes([96; 32]),
                not_before_ms: NOW as u64,
                expires_at_ms: NOW as u64 + 10_000,
            },
            RelyingParty::new("client.example", "https://client.example").unwrap(),
            &[7; 32],
            AuthenticationCeremony::IdentifiedAccount,
            vec![],
            NOW as u64,
        )
        .unwrap();
        let mut auth = crate::webauthn::tests::auth(&pending, 0x5d, 7);
        auth.extend([0; 16]);
        auth.extend(32u16.to_be_bytes());
        auth.extend([9; 32]);
        auth.extend(crate::webauthn::tests::cose());
        let object = Value::Map(vec![
            (Value::Text("fmt".into()), Value::Text("none".into())),
            (Value::Text("authData".into()), Value::Bytes(auth)),
            (Value::Text("attStmt".into()), Value::Map(vec![])),
        ]);
        let mut bytes = Vec::new();
        coset::cbor::ser::into_writer(&object, &mut bytes).unwrap();
        let response = RegistrationResponse {
            id: URL_SAFE_NO_PAD.encode([9; 32]),
            raw_id: URL_SAFE_NO_PAD.encode([9; 32]),
            credential_type: "public-key".into(),
            response: AttestationResponse {
                client_data_json: URL_SAFE_NO_PAD
                    .encode(crate::webauthn::tests::client(&pending, "webauthn.create")),
                attestation_object: URL_SAFE_NO_PAD.encode(bytes),
                transports: vec![],
            },
            client_extension_results: serde_json::json!({}),
        };
        let verified = verify_passkey_registration(&pending, &response, NOW as u64).unwrap();
        let prepared = prepare_first_authenticator_registration(
            &backup,
            &pins,
            &bundle,
            &signed_intent,
            &verified,
            NOW,
        )
        .unwrap();
        let request = prepared.request_json().unwrap();
        assert!(!request.contains("secret_hex"));
        let wire: serde_json::Value = serde_json::from_str(&request).unwrap();
        let record: Signed<AuthenticatorRecord> =
            serde_json::from_value(wire["record"].clone()).unwrap();
        assert_eq!(
            record.payload.initial_state,
            AuthenticatorState {
                sign_count: 7,
                backup_state: true
            }
        );
        assert_eq!(record.payload.nonce, pending.envelope().nonce);
        let proposal: Signed<AccountDocument> =
            serde_json::from_value(wire["proposal"].clone()).unwrap();
        let previous = proposal.payload.previous.clone().unwrap();
        let commit = Signed::sign(
            PolicyCommit {
                version: AUTH_VERSION,
                previous: previous.clone(),
                proposal_hash: proposal.hash().unwrap(),
                accepted_at_ms: NOW as u64,
                issued_at_ms: NOW as u64,
                expires_at_ms: NOW as u64 + 10_000,
                nonce: Nonce::from_bytes([97; 32]),
                dependencies: vec![previous.clone()],
            },
            &key(4),
        )
        .unwrap();
        let receipt = Signed::sign(
            FreshnessReceipt {
                version: AUTH_VERSION,
                current: previous,
                challenge_hash: commit.challenge_hash().unwrap(),
                issued_at_ms: NOW as u64,
                expires_at_ms: NOW as u64 + 10_000,
            },
            &key(4),
        )
        .unwrap();
        let mut committed = Committed {
            proposal,
            commit,
            receipts: vec![receipt],
        };
        let delta = vec![
            JournalEntry::Authenticator(record.clone()),
            JournalEntry::AccountCommit(committed.clone()),
        ];
        let finished = finish_first_authenticator_registration(
            &prepared,
            &serde_json::to_string(&delta).unwrap(),
            NOW,
        )
        .unwrap();
        let late = finish_first_authenticator_registration(
            &prepared,
            &serde_json::to_string(&delta).unwrap(),
            END + 1.0,
        )
        .unwrap();
        assert_eq!(late.pins_json().unwrap(), finished.pins_json().unwrap());
        assert!(
            finish_first_authenticator_registration(
                &prepared,
                &serde_json::to_string(&delta).unwrap(),
                NOW - 1.0
            )
            .is_err()
        );
        let updated_pins: Vec<PolicyPin> =
            serde_json::from_str(&finished.pins_json().unwrap()).unwrap();
        let updated_bundle: ClientAuthorizationBundle =
            serde_json::from_str(&finished.bundle_json().unwrap()).unwrap();
        let state = replay_policies(&updated_pins, &updated_bundle.journal).unwrap();
        assert_eq!(
            state
                .verify_authenticator(&record, NOW as u64)
                .unwrap()
                .state_floor(),
            record.payload.initial_state
        );
        assert_eq!(
            updated_pins
                .iter()
                .find(|p| p.domain == PolicyDomain::Account(intent.payload.account.clone()))
                .unwrap()
                .minimum_head
                .as_ref(),
            state.head(&PolicyDomain::Account(intent.payload.account.clone()))
        );
        assert!(
            build_first_authenticator_enrollment_intent(
                &backup,
                &finished.pins_json().unwrap(),
                &finished.bundle_json().unwrap(),
                &key(3),
                NOW,
                END
            )
            .is_err()
        );
        assert!(
            finish_first_authenticator_registration(
                &prepared,
                &prepared.completion_journal_json().unwrap(),
                NOW
            )
            .is_err()
        );
        let registered = enroll_nodes(
            CreatedAccount {
                pins: updated_pins.clone(),
                bundle: updated_bundle.clone(),
                authorization: verify_client_policy_state(
                    state.clone(),
                    &updated_bundle,
                    &updated_pins,
                    created.authorization.scope.account_id.clone(),
                    created.authorization.scope.space_id.clone(),
                    PublicKey::of(&key(3)),
                    NOW as u64,
                )
                .unwrap(),
            },
            12,
        );
        let registered_material = material(&registered);
        let private = material_state(&read_material(&registered_material).unwrap()).unwrap();
        assert_eq!(
            private
                .verify_authenticator(&record, NOW as u64)
                .unwrap()
                .state_floor(),
            state
                .verify_authenticator(&record, NOW as u64)
                .unwrap()
                .state_floor(),
        );
        let state = replay_policies(&registered.pins, &registered.bundle.journal).unwrap();
        let mut removed = registered;
        let mut document = state.account(&intent.payload.account).unwrap().clone();
        document.previous = state
            .head(&PolicyDomain::Account(intent.payload.account.clone()))
            .cloned();
        document.sequence += 1;
        document.authenticators.clear();
        let document = Signed::sign(document, &key(1)).unwrap();
        let mut removed_state = state.clone();
        let head = removed_state.install_account(document.clone()).unwrap();
        removed.bundle.journal.push(JournalEntry::Account(document));
        removed
            .pins
            .iter_mut()
            .find(|pin| pin.domain == head.domain)
            .unwrap()
            .minimum_head = Some(head.clone());
        removed.authorization = verify_client_policy_state(
            removed_state,
            &removed.bundle,
            &removed.pins,
            removed.authorization.scope.account_id.clone(),
            removed.authorization.scope.space_id.clone(),
            PublicKey::of(&key(3)),
            NOW as u64,
        )
        .unwrap();
        let removed_material = material(&removed);
        let private = material_state(&read_material(&removed_material).unwrap()).unwrap();
        assert!(
            private
                .account(&intent.payload.account)
                .unwrap()
                .authenticators
                .is_empty()
        );
        assert!(
            private
                .account_history(&intent.payload.account)
                .unwrap()
                .iter()
                .any(|doc| {
                    doc.payload.root_generation == 0 && !doc.payload.authenticators.is_empty()
                })
        );
        assert!(
            build_first_authenticator_enrollment_intent(
                &removed_material,
                &removed.pins_json().unwrap(),
                &removed.bundle_json().unwrap(),
                &key(3),
                NOW,
                END,
            )
            .is_err()
        );
        committed.receipts.clear();
        let failed = vec![
            JournalEntry::Authenticator(record),
            JournalEntry::AccountCommit(committed),
        ];
        assert!(
            finish_first_authenticator_registration(
                &prepared,
                &serde_json::to_string(&failed).unwrap(),
                NOW
            )
            .is_err()
        );
        if let Ok(path) = std::env::var("VIBEFACE_FIRST_REGISTRATION_VECTOR") {
            std::fs::write(path, serde_json::to_vec(&serde_json::json!({"now":NOW,"end":END,"material":backup,"pins":created.pins,"bundle":created.bundle,"intent":intent,"pending":pending,"response":response,"request":wire,"committed_journal":delta,"updated_pins":updated_pins,"updated_bundle":updated_bundle})).unwrap()).unwrap();
        }
    }

    #[test]
    fn additional_node_keeps_the_account_and_needs_root_possession_and_authority() {
        use node_enrollment::*;
        let original = account();
        let backup = material(&original);
        let pins = original.pins_json().unwrap();
        let bundle = original.bundle_json().unwrap();
        let new_claim = claim(7, 44, NOW);
        let prepared =
            prepare_account_node_enrollment(&backup, &pins, &bundle, &key(3), &new_claim, END, NOW)
                .unwrap();
        let request = prepared.request_json().unwrap();
        assert!(!request.contains("secret_hex"));
        let possession_request: AccountNodePossessionRequest =
            serde_json::from_str(&request).unwrap();
        let signed = possession_request.intent.clone();
        assert_eq!(
            signed.payload.certificate.owner,
            PolicyDomain::Account(original.authorization.scope.account_id.clone())
        );
        assert_eq!(
            signed.payload.certificate.uses,
            [DeviceUse::Noise, DeviceUse::ServiceInvocation]
        );
        assert!(
            sign_account_node_enrollment_possession(&request, &key(8), &nonce(44), NOW).is_err()
        );
        assert!(
            sign_account_node_enrollment_possession(&request, &key(7), &nonce(43), NOW).is_err()
        );
        assert!(
            sign_account_node_enrollment_possession(&request, &key(7), &nonce(44), NOW + 100_000.0)
                .is_err()
        );
        let possession =
            sign_account_node_enrollment_possession(&request, &key(7), &nonce(44), NOW).unwrap();
        assert!(
            prepare_account_node_enrollment_commit(&prepared, &backup, &"00".repeat(64), NOW)
                .is_err()
        );
        let commit =
            prepare_account_node_enrollment_commit(&prepared, &backup, &possession, NOW).unwrap();
        let public = commit.request_json().unwrap();
        assert!(commit_account_node_enrollment(&public, &key(1), &pins, &bundle, NOW).is_err());
        let delta = commit_account_node_enrollment(&public, &key(4), &pins, &bundle, NOW).unwrap();
        let result = finish_account_node_enrollment(&commit, &delta, NOW).unwrap();
        let delayed = finish_account_node_enrollment(&commit, &delta, NOW + 200_000.0).unwrap();
        assert_eq!(delayed.pins_json().unwrap(), result.pins_json().unwrap());
        assert_eq!(result.account(), &original.authorization.scope.account_id);
        let before = replay_policies(&original.pins, &original.bundle.journal).unwrap();
        let after = replay_policies(result.pins(), result.journal()).unwrap();
        let cert = after
            .verify_transport_device(&DeviceId::from_key(&PublicKey::of(&key(7))), NOW as u64)
            .unwrap();
        assert_eq!(
            cert.payload.owner,
            PolicyDomain::Account(result.account().clone())
        );
        assert_eq!(before.grants.len(), after.grants.len());
        assert_eq!(
            before.account(result.account()).unwrap().enrollment,
            after.account(result.account()).unwrap().enrollment
        );
        assert_eq!(result.bundle().grant_leaf, original.bundle.grant_leaf);
        let expired_proof = finish_account_node_enrollment(&commit, &delta, END + 1.0).unwrap();
        assert_eq!(
            expired_proof.pins_json().unwrap(),
            result.pins_json().unwrap()
        );
        assert!(
            verify_account_node_installation(
                &expired_proof.installation_json().unwrap(),
                &key(7),
                &nonce(44),
                END + 1.0
            )
            .is_err()
        );
        let installation = result.installation_json().unwrap();
        assert!(!installation.contains("secret_hex"));
        let installed =
            verify_account_node_installation(&installation, &key(7), &nonce(44), NOW + 200_000.0)
                .unwrap();
        assert_eq!(
            installed.bundle_json().unwrap(),
            result.bundle_json().unwrap()
        );
        assert!(verify_account_node_installation(&installation, &key(8), &nonce(44), NOW).is_err());
        assert!(verify_account_node_installation(&installation, &key(7), &nonce(43), NOW).is_err());
        assert!(
            finish_account_node_enrollment(
                &commit,
                &commit.completion_journal_json().unwrap(),
                NOW
            )
            .is_err()
        );
        let mut wrong = signed.payload;
        wrong.certificate.uses.push(DeviceUse::AccountInvocation);
        let wrong = AccountNodePossessionRequest {
            intent: Signed::sign(wrong, &key(1)).unwrap(),
            ..possession_request
        };
        assert!(
            sign_account_node_enrollment_possession(
                &encode(&wrong).unwrap(),
                &key(7),
                &nonce(44),
                NOW
            )
            .is_err()
        );
        let mut changed_context: AccountNodePossessionRequest =
            serde_json::from_str(&request).unwrap();
        changed_context.bundle.grant_leaf = ObjectHash::body(&"another public context").unwrap();
        assert!(
            sign_account_node_enrollment_possession(
                &encode(&changed_context).unwrap(),
                &key(7),
                &nonce(44),
                NOW
            )
            .unwrap_err()
            .contains("trust context")
        );
        let mut changed_installation: serde_json::Value =
            serde_json::from_str(&installation).unwrap();
        changed_installation["pins"] = serde_json::json!([]);
        assert!(
            verify_account_node_installation(
                &encode(&changed_installation).unwrap(),
                &key(7),
                &nonce(44),
                NOW
            )
            .unwrap_err()
            .contains("trust context")
        );
        assert!(
            after
                .verify_account_client_device(
                    result.account(),
                    &DeviceId::from_key(&PublicKey::of(&key(3))),
                    NOW as u64
                )
                .is_ok()
        );
        assert!(
            after
                .verify_account_client_device(
                    result.account(),
                    &DeviceId::from_key(&PublicKey::of(&key(4))),
                    NOW as u64
                )
                .is_err()
        );
        for reused in 1..=4 {
            assert!(
                prepare_account_node_enrollment(
                    &backup,
                    &pins,
                    &bundle,
                    &key(3),
                    &claim(reused, 44, NOW),
                    END,
                    NOW
                )
                .is_err()
            );
        }
        if let Ok(path) = std::env::var("VIBEFACE_NODE_ENROLLMENT_VECTOR") {
            std::fs::write(path, serde_json::to_vec(&serde_json::json!({"now":NOW,"end":END,"material":backup,"pins":original.pins,"bundle":original.bundle,"claim":new_claim.claim,"request":serde_json::from_str::<serde_json::Value>(&request).unwrap(),"possession":possession,"commit_request":serde_json::from_str::<serde_json::Value>(&public).unwrap(),"delta":serde_json::from_str::<serde_json::Value>(&delta).unwrap(),"installation":serde_json::from_str::<serde_json::Value>(&installation).unwrap(),"updated_pins":result.pins(),"updated_bundle":result.bundle()})).unwrap()).unwrap();
        }
    }

    #[test]
    fn cross_node_recovery_requires_live_source_cas_two_possessions_and_exact_read_only_history() {
        use super::cross_node::*;
        let mut original = account();
        let backup = material(&original);
        let mut source_state = replay_policies(&original.pins, &original.bundle.journal).unwrap();
        let original_space = original.authorization.scope.space_id.clone();
        let first_hash = source_state.space(&original_space).unwrap().resources[0].clone();
        let first_resource = source_state
            .resources
            .get(&first_hash)
            .unwrap()
            .payload
            .resource
            .clone();
        let next_resource = ResourceId::random().unwrap();
        let record = (0..128)
            .find_map(|_| {
                let record = Signed::sign(
                    ResourceRecord {
                        version: AUTH_VERSION,
                        space: original_space.clone(),
                        resource: next_resource.clone(),
                        kind: ResourceKind::Project,
                        host: ServiceId::from_key(&PublicKey::of(&key(4))),
                        issued_under: source_state
                            .head(&PolicyDomain::Space(original_space.clone()))
                            .unwrap()
                            .clone(),
                        nonce: Nonce::random().unwrap(),
                    },
                    &key(1),
                )
                .unwrap();
                (record.hash().unwrap().cmp(&first_hash) != next_resource.cmp(&first_resource))
                    .then_some(record)
            })
            .expect("fixture must put ObjectHash and ResourceId in opposite orders");
        let hash = source_state.install_resource(record.clone()).unwrap();
        let mut space = source_state.space(&original_space).unwrap().clone();
        space.previous = source_state
            .head(&PolicyDomain::Space(original_space.clone()))
            .cloned();
        space.sequence += 1;
        space.resources.push(hash);
        space.resources.sort();
        let space = Signed::sign(space, &key(1)).unwrap();
        let head = source_state.install_space(space.clone()).unwrap();
        original
            .bundle
            .journal
            .extend([JournalEntry::Resource(record), JournalEntry::Space(space)]);
        original
            .pins
            .iter_mut()
            .find(|pin| pin.domain == head.domain)
            .unwrap()
            .minimum_head = Some(head.clone());
        let old_state = replay_policies(&original.pins, &original.bundle.journal).unwrap();
        let inspected: serde_json::Value =
            serde_json::from_str(&inspect_cross_node_recovery_sources(&backup, RESTORE).unwrap())
                .unwrap();
        assert_eq!(inspected["source_projects"].as_array().unwrap().len(), 1);
        let mut current_resources: Vec<_> = old_state
            .space(&original_space)
            .unwrap()
            .resources
            .iter()
            .filter_map(|hash| old_state.resources.get(hash))
            .map(|record| ResourceRef {
                space: record.payload.space.clone(),
                resource: record.payload.resource.clone(),
            })
            .collect();
        current_resources.sort_by(|a, b| a.resource.cmp(&b.resource));
        assert_eq!(current_resources.len(), 2);
        let resources = encode(&current_resources).unwrap();
        let original_request = build_cross_node_recovery_request(
            &backup,
            &key(5),
            &key(6),
            &claim(7, 44, RESTORE),
            &resources,
            END - 1000.0,
            RESTORE,
            RESTORE + 100_000.0,
        )
        .unwrap();
        let signed: Signed<CrossNodeRecoveryRequest> =
            serde_json::from_str(&original_request).unwrap();
        let missing_resources = encode(&vec![ResourceRef {
            space: original_space.clone(),
            resource: ResourceId::from_bytes([199; 32]),
        }])
        .unwrap();
        let missing_request: Signed<CrossNodeRecoveryRequest> = serde_json::from_str(
            &build_cross_node_recovery_request(
                &backup,
                &key(5),
                &key(6),
                &claim(7, 44, RESTORE),
                &missing_resources,
                END - 1000.0,
                RESTORE,
                RESTORE + 100_000.0,
            )
            .unwrap(),
        )
        .unwrap();
        assert!(
            create_cross_node_recovery_witness(
                &old_state,
                &missing_request,
                &original.bundle,
                &key(4),
                RESTORE as u64
            )
            .is_err()
        );
        let alien = encode(&vec![ResourceRef {
            space: SpaceId::personal(&AccountId::from_root(&PublicKey::of(&key(8)))),
            resource: current_resources[0].resource.clone(),
        }])
        .unwrap();
        assert!(
            build_cross_node_recovery_request(
                &backup,
                &key(5),
                &key(6),
                &claim(7, 44, RESTORE),
                &alien,
                END - 1000.0,
                RESTORE,
                RESTORE + 100_000.0
            )
            .is_err()
        );
        let context = create_cross_node_recovery_witness(
            &old_state,
            &signed,
            &original.bundle,
            &key(4),
            RESTORE as u64,
        )
        .unwrap();
        let verified = verify_cross_node_recovery_context(
            &encode(&context).unwrap(),
            &original_request,
            RESTORE,
        )
        .unwrap();
        let prepared =
            prepare_cross_node_account_recovery(&backup, &verified, &key(5), &key(6), END, RESTORE)
                .unwrap();
        let possession: CrossNodeRecoveryPossessionRequest =
            serde_json::from_str(&prepared.possession_request_json().unwrap()).unwrap();
        let c_proof = sign_cross_node_recovery_possession(
            &possession,
            &key(7),
            &Nonce::parse(&nonce(44)).unwrap(),
            RESTORE as u64,
        )
        .unwrap();
        let b_proof = sign_cross_node_recovery_source_possession(
            &old_state,
            &possession,
            &key(4),
            RESTORE as u64,
        )
        .unwrap();
        let mut pending = finish_cross_node_account_recovery(
            &prepared,
            &key(5),
            c_proof.as_str(),
            b_proof.as_str(),
            RESTORE,
        )
        .unwrap();
        let pending_material = pending
            .recovery_material_json(&key(5), &backup, "https://target.example", RESTORE)
            .unwrap();
        assert!(pending_material.len() > 64 * 1024);
        let sealed = crate::recovery::recovery_seal(
            pending_material.as_bytes(),
            "synthetic recovery package password",
        )
        .unwrap();
        let opened =
            crate::recovery::recovery_open(&sealed, "synthetic recovery package password").unwrap();
        assert_eq!(opened, pending_material.as_bytes());
        let restored =
            restore_prepared_cross_node_recovery_commit(&pending_material, RESTORE + 200_000.0)
                .unwrap();
        assert_eq!(
            restored.request_json().unwrap(),
            pending.request_json().unwrap()
        );
        assert_eq!(
            restored.client_identity().unwrap().public_key_hex(),
            key(6).public_key_hex()
        );
        let resume: serde_json::Value =
            serde_json::from_str(&restored.resume_context_json().unwrap()).unwrap();
        assert_eq!(resume["source"]["node_public_key"], key(4).public_key_hex());
        assert_eq!(resume["target"]["node_public_key"], key(7).public_key_hex());
        let request: CrossNodeRecoveryCommitRequest =
            serde_json::from_str(&pending.request_json().unwrap()).unwrap();
        let verified_commit = verify_cross_node_recovery_commit(
            &old_state,
            &request,
            &PublicKey::of(&key(4)),
            RESTORE as u64,
        )
        .unwrap();
        assert_eq!(
            verified_commit.request_hash().unwrap(),
            ObjectHash::body(&signed).unwrap()
        );
        assert_eq!(
            verified_commit.possession_hash().unwrap(),
            ObjectHash::body(&possession).unwrap()
        );
        let committed =
            accept_cross_node_recovery_commit(&verified_commit, &key(4), RESTORE as u64).unwrap();
        let committed_json = encode(&committed).unwrap();
        let request_wire = serde_json::json!({"id":"cross-native","method":"identity/cross_node_recovery_commit","params":request});
        let committed_wire =
            serde_json::json!({"id":"cross-native","result":{"committed":committed}});
        assert!(canonical(&request_wire).unwrap().len() <= MAX_PROOF_BYTES);
        assert!(canonical(&committed_wire).unwrap().len() <= MAX_PROOF_BYTES);
        let completed =
            finish_cross_node_recovery_commit(&restored, &committed_json, RESTORE + 200_000.0)
                .unwrap();
        assert_eq!(
            completed.pins_json().unwrap(),
            encode(&request.pins).unwrap()
        );
        let final_material = completed
            .final_recovery_material_json("https://target.example", RESTORE + 200_000.0)
            .unwrap();
        inspect_account_recovery_material(&final_material, RESTORE + 200_000.0).unwrap();
        let installed = verify_cross_node_recovery_installation(
            &committed,
            &PublicKey::of(&key(7)),
            &Nonce::parse(&nonce(44)).unwrap(),
            RESTORE as u64,
        )
        .unwrap();
        let after = replay_policies(installed.pins(), &installed.bundle().journal).unwrap();
        let recovered_scope: ClientScope =
            serde_json::from_str(&completed.scope_json().unwrap()).unwrap();
        let c_projection = after
            .client_authorization_projection(
                installed.pins(),
                &installed.bundle().journal,
                &recovered_scope.account_id,
                &recovered_scope.space_id,
                &PublicKey::of(&key(6)),
                &installed.bundle().grant_leaf,
                RESTORE as u64,
            )
            .unwrap();
        let c_auth = verify_client_authorization(
            &encode(&c_projection).unwrap(),
            &completed.pins_json().unwrap(),
            recovered_scope.account_id.as_str(),
            recovered_scope.space_id.as_str(),
            &key(6).public_key_hex(),
            RESTORE,
        )
        .unwrap();
        assert_eq!(c_auth.audience_public_key(), &PublicKey::of(&key(7)));
        assert_eq!(c_auth.resources().len(), 1);
        assert_eq!(
            c_auth.resources()[0].payload.host,
            ServiceId::from_key(&PublicKey::of(&key(7)))
        );
        let selections: serde_json::Value =
            serde_json::from_str(&completed.source_read_selections_json().unwrap()).unwrap();
        assert_eq!(selections.as_array().unwrap().len(), 2);
        let source_bundle: ClientAuthorizationBundle =
            serde_json::from_value(selections[0]["authorization"].clone()).unwrap();
        let old_scope = original.authorization.scope();
        let source_auth = verify_client_authorization(
            &encode(&source_bundle).unwrap(),
            &completed.pins_json().unwrap(),
            old_scope.account_id.as_str(),
            old_scope.space_id.as_str(),
            &key(6).public_key_hex(),
            RESTORE,
        )
        .unwrap();
        assert_eq!(source_auth.audience_public_key(), &PublicKey::of(&key(4)));
        let resource: ResourceRef =
            serde_json::from_value(selections[0]["project"].clone()).unwrap();
        let mut invocation = Invocation {
            version: AUTH_VERSION,
            actor: Actor::Account(old_scope.account_id.clone()),
            device: source_auth.scope.device_id.clone(),
            initiator: Actor::Account(old_scope.account_id.clone()),
            initiating_grant: None,
            resource: resource.clone(),
            action: Action::ConversationRead,
            audience: source_auth.audience.clone(),
            presenter: source_auth.scope.device_id.clone(),
            operation_id: "synthetic-source-read".to_owned(),
            body_hash: ObjectHash::body(&serde_json::json!({})).unwrap(),
            grant_leaf: source_bundle.grant_leaf.clone(),
            expected_policies: source_auth.heads.clone(),
            context: InvocationContext::default(),
            nonce: Nonce::from_bytes([55; 32]),
            not_before_ms: RESTORE as u64,
            expires_at_ms: RESTORE as u64 + 1000,
        };
        let transport = TransportEvidence::DirectNoise {
            device: source_auth.scope.device_id.clone(),
        };
        let facts = |value: &Invocation| RequestFacts {
            resource: value.resource.clone(),
            action: value.action,
            audience: value.audience.clone(),
            operation_id: value.operation_id.clone(),
            body_hash: value.body_hash.clone(),
            context: value.context.clone(),
        };
        let read = after
            .verify_invocation(
                &Signed::sign(invocation.clone(), &key(6)).unwrap(),
                &facts(&invocation),
                &transport,
                RESTORE as u64,
            )
            .unwrap();
        assert_eq!(read.dependencies().len(), 2);
        for action in [
            Action::MessageAppend,
            Action::TaskCreate,
            Action::AgentStart,
            Action::ExecutionOpen,
            Action::MemberManage,
            Action::AttachmentWrite,
            Action::OperationResolve,
        ] {
            invocation.action = action;
            assert!(
                after
                    .verify_invocation(
                        &Signed::sign(invocation.clone(), &key(6)).unwrap(),
                        &facts(&invocation),
                        &transport,
                        RESTORE as u64
                    )
                    .is_err()
            );
        }
        assert_eq!(
            source_auth
                .policy_json()
                .unwrap()
                .contains("message_append"),
            false
        );
        assert_eq!(
            after
                .account(&old_scope.account_id)
                .unwrap()
                .authority
                .public_key,
            PublicKey::of(&key(7))
        );
        assert_eq!(after.space(&old_scope.space_id).unwrap().resources.len(), 3);
        assert!(
            after
                .verify_account_client_device(
                    &old_scope.account_id,
                    &DeviceId::from_key(&PublicKey::of(&key(3))),
                    RESTORE as u64
                )
                .is_err()
        );
        assert!(
            verify_cross_node_recovery_commit(
                &after,
                &request,
                &PublicKey::of(&key(4)),
                RESTORE as u64
            )
            .is_err()
        );
        for domain in [
            PolicyDomain::Account(old_scope.account_id.clone()),
            PolicyDomain::Space(old_scope.space_id.clone()),
        ] {
            let mut advanced = old_state.clone();
            match domain {
                PolicyDomain::Account(account) => {
                    let mut document = advanced.account(&account).unwrap().clone();
                    document.previous = advanced.head(&PolicyDomain::Account(account)).cloned();
                    document.sequence += 1;
                    advanced
                        .install_account(Signed::sign(document, &key(1)).unwrap())
                        .unwrap();
                }
                PolicyDomain::Space(space) => {
                    let mut document = advanced.space(&space).unwrap().clone();
                    document.previous = advanced.head(&PolicyDomain::Space(space)).cloned();
                    document.sequence += 1;
                    advanced
                        .install_space(Signed::sign(document, &key(1)).unwrap())
                        .unwrap();
                }
            }
            assert!(
                verify_cross_node_recovery_commit(
                    &advanced,
                    &request,
                    &PublicKey::of(&key(4)),
                    RESTORE as u64
                )
                .is_err()
            );
        }
        assert!(
            sign_cross_node_recovery_possession(
                &possession,
                &key(4),
                &Nonce::parse(&nonce(44)).unwrap(),
                RESTORE as u64
            )
            .is_err()
        );
        assert!(
            sign_cross_node_recovery_possession(
                &possession,
                &key(7),
                &Nonce::parse(&nonce(45)).unwrap(),
                RESTORE as u64
            )
            .is_err()
        );
        assert!(
            sign_cross_node_recovery_source_possession(
                &old_state,
                &possession,
                &key(7),
                RESTORE as u64
            )
            .is_err()
        );
        assert!(
            finish_cross_node_account_recovery(
                &prepared,
                &key(5),
                b_proof.as_str(),
                c_proof.as_str(),
                RESTORE
            )
            .is_err()
        );
        assert!(
            finish_cross_node_recovery_commit(&restored, &committed_json, RESTORE - 1.0).is_err()
        );
        assert!(
            verify_cross_node_recovery_installation(
                &committed,
                &PublicKey::of(&key(7)),
                &Nonce::parse(&nonce(44)).unwrap(),
                END as u64 + 1
            )
            .is_err()
        );
        let expired =
            finish_cross_node_recovery_commit(&restored, &committed_json, END + 1.0).unwrap();
        assert_eq!(expired.scope_json().unwrap(), "null");
        for mutation in 0..5 {
            let mut changed = context.clone();
            match mutation {
                0 => {
                    changed.witness =
                        Signed::sign(changed.witness.payload.clone(), &key(7)).unwrap()
                }
                1 => changed.request.payload.new_device_public_key = PublicKey::of(&key(8)),
                2 => changed.bundle.journal.pop().map(|_| ()).unwrap(),
                3 => changed.request.payload.saved_pins[0].genesis_root = PublicKey::of(&key(8)),
                4 => changed.witness.payload.expected_account_head.sequence += 1,
                _ => unreachable!(),
            }
            assert!(
                verify_cross_node_recovery_context(
                    &encode(&changed).unwrap(),
                    &original_request,
                    RESTORE
                )
                .is_err()
            );
        }
        let mut changed = request.clone();
        changed.journal_delta.retain(|entry| !matches!(entry,JournalEntry::Device(c) if c.payload.public_key==PublicKey::of(&key(4)) && c.issuer==PublicKey::of(&key(5))));
        assert!(
            verify_cross_node_recovery_commit(
                &old_state,
                &changed,
                &PublicKey::of(&key(4)),
                RESTORE as u64
            )
            .is_err()
        );
        let mut oversized = request.clone();
        oversized
            .journal_delta
            .extend(std::iter::repeat_n(request.journal_delta[0].clone(), 200));
        assert!(matches!(
            verify_cross_node_recovery_commit(
                &old_state,
                &oversized,
                &PublicKey::of(&key(4)),
                RESTORE as u64
            ),
            Err(AuthorizationError::InvalidProof(
                "cross-node recovery requires a bounded checkpoint"
            ))
        ));
        let mut pending_value: serde_json::Value = serde_json::from_str(&pending_material).unwrap();
        pending_value["client_secret_hex"] = key(8).secret_hex().into();
        assert!(
            restore_prepared_cross_node_recovery_commit(&pending_value.to_string(), RESTORE)
                .is_err()
        );
        if let Ok(path) = std::env::var("VIBEFACE_CROSS_NODE_RECOVERY_VECTOR") {
            std::fs::write(path,encode(&serde_json::json!({"material":backup,"original_bundle":original.bundle,"original_pins":original.pins,
                "request":signed,"context":context,"possession":possession,"commit_request":request,"committed":committed,
                "pending_material":pending_material,"read_selections":selections,"now_ms":RESTORE,"expires_at_ms":END,
                "request_wire_bytes":canonical(&request_wire).unwrap().len(),"committed_wire_bytes":canonical(&committed_wire).unwrap().len()})).unwrap()).unwrap();
        }
    }
}
