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
    checkpoint_recovery_material_in_state, compact_recovery_material_in_state, material_state,
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
    before_state: VerifiedPolicySet,
    verified_context: Option<stream::VerifiedRecoveryPolicyContext>,
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

#[cfg(test)]
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

fn require_independent_new_recovery_keys(
    state: &VerifiedPolicySet,
    root: &PublicKey,
    client: &PublicKey,
) -> Result<()> {
    require(
        root != client
            && !state.roots.values().any(|key| key == root || key == client)
            && !state.devices.values().any(|value| {
                &value.payload.public_key == root || &value.payload.public_key == client
            })
            && !state
                .accounts
                .values()
                .flat_map(|account| &account.history)
                .any(|value| {
                    value.payload.recovery.keys.contains(root)
                        || value.payload.recovery.keys.contains(client)
                }),
        "recovery requires independent root and client keys unused by historical policy",
    )
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
    context: &stream::VerifiedRecoveryPolicyContext,
    next_root: &Identity,
    next_client: &Identity,
    certificate_expires_at_ms: f64,
    now_ms: f64,
) -> crate::Result<PreparedAccountRecovery> {
    let _scope = crate::signature_verification::ScopedSignatureVerification::new();
    let prepare = || -> Result<PreparedAccountRecovery> {
        let material = read_material(material_json)?;
        let recovery = validate_material(&material)?;
        let now = timestamp(now_ms)?;
        let read = context.request();
        require(
            read.kind() == stream::RecoveryContextKind::Account
                && read.account() == &material.scope.account_id
                && read.space() == &material.scope.space_id
                && read.saved_pins() == material.pins
                && read.source_node_public_key() == &material.entry.node_public_key
                && read.new_device_public_key() == &PublicKey::of(next_client),
            "recovery context changed the saved material or new client",
        )?;
        validate_recovery_context_current(context, now)?;
        let mut prepared = prepare_account_recovery_in_state(
            &material,
            &recovery,
            context.verified_state().clone(),
            context.policy().pins(),
            context.policy().journal(),
            next_root,
            next_client,
            &context.snapshot().payload.claim,
            timestamp(certificate_expires_at_ms)?,
            now,
            None,
        )?;
        prepared.verified_context = Some(context.clone());
        Ok(prepared)
    };
    prepare().map_err(|error| error.to_string())
}

fn validate_recovery_context_current(
    context: &stream::VerifiedRecoveryPolicyContext,
    now: u64,
) -> Result<()> {
    let snapshot = &context.snapshot().payload;
    validate_window(snapshot.issued_at_ms, snapshot.expires_at_ms, now)?;
    let witness = &context.witness().payload;
    validate_window(witness.issued_at_ms, witness.expires_at_ms, now)?;
    let read = context.request();
    read.verify(context.verified_state(), read.source_node_public_key(), now)
}

#[cfg(test)]
pub(super) fn prepare_account_recovery_from_bundle(
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

#[cfg(test)]
#[allow(clippy::too_many_arguments)]
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
    let _scope = crate::signature_verification::ScopedSignatureVerification::new();
    let prepare = || -> Result<PreparedAccountRecovery> {
        let material = read_material(material_json)?;
        let recovery = validate_material(&material)?;
        let now_ms = timestamp(now_ms)?;
        let expires_at_ms = timestamp(certificate_expires_at_ms)?;
        let latest = read_bundle(latest_bundle_json)?;
        require(
            latest.journal.len() <= 118,
            "recovery journal requires checkpoint",
        )?;
        let state = replay_policies(&material.pins, &latest.journal)?;
        prepare_account_recovery_in_state(
            &material,
            &recovery,
            state,
            &material.pins,
            &latest.journal,
            next_root,
            next_client,
            &claim.claim,
            expires_at_ms,
            now_ms,
            preserved_source,
        )
    };
    prepare().map_err(|e| e.to_string())
}

#[allow(clippy::too_many_arguments)]
fn prepare_account_recovery_in_state(
    material: &RecoveryMaterial,
    recovery: &Identity,
    before_state: VerifiedPolicySet,
    pins: &[PolicyPin],
    journal: &[JournalEntry],
    next_root: &Identity,
    next_client: &Identity,
    claim: &Signed<NodeClaim>,
    expires_at_ms: u64,
    now_ms: u64,
    preserved_source: Option<&ServiceId>,
) -> Result<PreparedAccountRecovery> {
    validate_window(now_ms, expires_at_ms, now_ms)?;
    let node_key = claim.payload.node_public_key.clone();
    validate_claim(claim, &node_key, &claim.payload.claim_nonce, now_ms)?;
    let mut state = before_state.clone();
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
            && account.recovery.keys == [PublicKey::of(recovery)],
        "recovery key is no longer current",
    )?;
    let root_key = PublicKey::of(next_root);
    let client_key = PublicKey::of(next_client);
    require_independent_new_recovery_keys(&state, &root_key, &client_key)?;
    require(
        BTreeSet::from([
            root_key.clone(),
            client_key.clone(),
            node_key.clone(),
            PublicKey::of(recovery),
        ])
        .len()
            == 4
            && state
                .account_history(&account.account)
                .is_some_and(|history| {
                    history.iter().all(|a| {
                        a.payload.current_root != root_key && a.payload.current_root != client_key
                    })
                })
            && !state
                .devices
                .values()
                .any(|c| c.payload.public_key == client_key || c.payload.public_key == root_key),
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
    recovered.approve_recovery(recovery)?;
    recovered.accept_root(next_root)?;
    let account_recovery = Signed::sign(recovered, recovery)?;
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
        claim.payload.claim_nonce.clone(),
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
            claim: claim.clone(),
            expected_account_head,
            expected_space_head,
            account_recovery,
            node_certificate,
        },
        pins: pins.to_vec(),
        journal: journal.to_vec(),
        before_state,
        verified_context: None,
        client_certificate,
        space_document,
        owner_grant,
        service_grant,
        new_project,
    })
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen)]
impl PreparedAccountRecovery {
    #[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen(js_name = nodeRequestJson))]
    pub fn node_request_json(&self) -> crate::Result<String> {
        if let Some(context) = &self.verified_context {
            encode(&stream::StreamedRecoveryNodeRequest {
                context: context.reference().map_err(|error| error.to_string())?,
                request: self.request.clone(),
            })
        } else {
            encode(&self.request)
        }
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
                && recovered.authorization.scope.space_id == self.space_document.space
                && recovered.authorization.scope.device_id
                    == self.client_certificate.payload.device,
            "recovery result belongs to another proposal",
        )
        .map_err(|e| e.to_string())?;
        if let Some(context) = &self.verified_context {
            return encode(&stream::StreamedRecoveryCommitRequest {
                context: context.reference().map_err(|error| error.to_string())?,
                request_hash: ObjectHash::body(&self.request).map_err(|error| error.to_string())?,
                expected_account_head: self.request.expected_account_head.clone(),
                expected_space_head: self.request.expected_space_head.clone(),
                journal_delta: recovered.bundle.journal[self.journal.len()..].to_vec(),
                grant_leaf: recovered.bundle.grant_leaf.clone(),
                new_device_public_key: self.client_certificate.payload.public_key.clone(),
                claim_nonce: self.request.claim.payload.claim_nonce.clone(),
            });
        }
        encode(&RecoveryCommit {
            expected_account_head: &self.request.expected_account_head,
            expected_space_head: &self.request.expected_space_head,
            journal_delta: &recovered.bundle.journal[self.journal.len()..],
            claim_nonce: &self.request.claim.payload.claim_nonce,
            request_hash: ObjectHash::body(&self.request).map_err(|e| e.to_string())?,
        })
    }
}

#[cfg(test)]
pub(super) fn sign_recovery_node_possession(
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
    let _scope = crate::signature_verification::ScopedSignatureVerification::new();
    let finish = || -> Result<CreatedAccount> {
        let now_ms = timestamp(now_ms)?;
        if let Some(context) = &prepared.verified_context {
            validate_recovery_context_current(context, now_ms)?;
        }
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
        let mut state = prepared.before_state.clone();
        let approved_account = stream::approval::apply_locally_approved_account_recovery(
            &mut state,
            &prepared.request.account_recovery,
            &prepared.request.expected_account_head,
        )?;
        let account_head = account_stamp(&prepared.request.account_recovery)?;
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
        let mut delta = vec![
            JournalEntry::Account(prepared.request.account_recovery.clone()),
            JournalEntry::Device(prepared.client_certificate.clone()),
            JournalEntry::Device(node_certificate),
            JournalEntry::Account(account),
            JournalEntry::Space(space_recovery),
        ];
        if let Some(record) = new_resource {
            delta.push(JournalEntry::Resource(record));
        }
        delta.extend([
            JournalEntry::Grant(grant),
            JournalEntry::Space(space_document),
            JournalEntry::Grant(prepared.service_grant.clone()),
            JournalEntry::Space(service_document),
        ]);
        let mut journal = prepared.journal.clone();
        journal.extend(delta.iter().cloned());
        let bundle = ClientAuthorizationBundle {
            journal,
            grant_leaf,
        };
        let mut authorization = verify_client_policy_state(
            state,
            &bundle,
            &pins,
            prepared.request.account_recovery.payload.account.clone(),
            prepared.space_document.space.clone(),
            prepared.client_certificate.payload.public_key.clone(),
            now_ms,
        )?;
        if let Some(context) = &prepared.verified_context {
            authorization.imported_snapshot = Some(
                context
                    .policy()
                    .apply_locally_approved_recovery_delta(&delta, &approved_account)?
                    .with_independent_pins(&pins)?,
            );
        }
        Ok(CreatedAccount {
            bundle,
            pins,
            authorization,
        })
    };
    finish().map_err(|error| error.to_string())
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

    fn import_node_enrollment(
        created: &CreatedAccount,
        request_json: &str,
        node: u8,
        claim_nonce: u8,
        stages: &mut Vec<serde_json::Value>,
    ) -> (
        node_enrollment::VerifiedAccountNodeEnrollmentContext,
        serde_json::Value,
    ) {
        use crate::authorization::enrollment_policy_import::{
            EnrollmentPolicyPage, EnrollmentPolicyPageRequest, EnrollmentPolicyReadRequest,
            EnrollmentPolicySnapshot, EnrollmentPolicyWitness,
            verify_enrollment_policy_read_request,
        };
        use node_enrollment::*;
        let envelope: serde_json::Value = serde_json::from_str(request_json).unwrap();
        let credential: AccountNodeEnrollmentCredential =
            serde_json::from_value(envelope["credential"]["credential"].clone()).unwrap();
        let request: Signed<EnrollmentPolicyReadRequest> =
            serde_json::from_value(envelope["request"].clone()).unwrap();
        let descriptor = &credential.descriptor;
        let credential_hash = ObjectHash::body(&credential).unwrap();
        let request_hash = request.hash().unwrap();
        stream::measured(stages, "node_source_read", || {
            let mut usage = verify_account_node_enrollment_read(
                &created.authorization.policy_snapshot,
                &credential.intent,
                descriptor,
                &PublicKey::of(&key(4)),
                &PublicKey::of(&key(3)),
                NOW as u64,
            )?;
            usage.merge(&verify_enrollment_policy_read_request(
                &request,
                descriptor,
                &credential_hash,
                &PublicKey::of(&key(4)),
                &PublicKey::of(&key(3)),
                NOW as u64,
            )?);
            usage.enforce_limits()
        });
        let snapshot = Signed::sign(
            EnrollmentPolicySnapshot {
                version: AUTH_VERSION,
                purpose: descriptor.purpose,
                credential_hash: credential_hash.clone(),
                request_hash: request_hash.clone(),
                snapshot_id: Nonce::from_bytes([77; 32]),
                descriptor_hash: ObjectHash::body(descriptor).unwrap(),
                reader_public_key: PublicKey::of(&key(3)),
                target_public_key: PublicKey::of(&key(node)),
                source_node_public_key: PublicKey::of(&key(4)),
                saved_pins_hash: ObjectHash::body(&descriptor.pins).unwrap(),
                required_heads_hash: ObjectHash::body(&descriptor.required_heads).unwrap(),
                target_heads: descriptor.target_heads.clone(),
                issued_at_ms: NOW as u64,
                expires_at_ms: descriptor.expires_at_ms,
            },
            &key(4),
        )
        .unwrap();
        let credential_json = encode(&credential).unwrap();
        let read_json = encode(&request).unwrap();
        let snapshot_json = encode(&snapshot).unwrap();
        let mut import = stream::measured(stages, "node_import_begin", || {
            begin_account_node_enrollment_import(
                &credential_json,
                &read_json,
                &snapshot_json,
                &key(node),
                &nonce(claim_nonce),
                &key(3).public_key_hex(),
                NOW,
            )
        });
        let snapshot_hash = snapshot.hash().unwrap();
        let mut previous = snapshot_hash.clone();
        let mut pages = Vec::new();
        for (index, entry) in created.bundle.journal.iter().enumerate() {
            let page_request: EnrollmentPolicyPageRequest =
                serde_json::from_str(&import.page_request_json().unwrap()).unwrap();
            assert_eq!(page_request.snapshot_id, snapshot.payload.snapshot_id);
            assert_eq!(page_request.request_hash, request_hash);
            let complete = index + 1 == created.bundle.journal.len();
            let page = Signed::sign(
                EnrollmentPolicyPage {
                    version: AUTH_VERSION,
                    purpose: descriptor.purpose,
                    credential_hash: credential_hash.clone(),
                    request_hash: request_hash.clone(),
                    snapshot_id: snapshot.payload.snapshot_id.clone(),
                    snapshot_hash: snapshot_hash.clone(),
                    request_cursor: page_request.cursor,
                    page_index: index as u64,
                    previous_page_hash: previous,
                    entry: Some(entry.clone()),
                    next_cursor: (!complete).then(|| {
                        ObjectHash::body(&("node-enrollment-cursor", &snapshot_hash, index))
                            .unwrap()
                    }),
                    complete,
                    issued_at_ms: NOW as u64,
                    expires_at_ms: descriptor.expires_at_ms,
                },
                &key(4),
            )
            .unwrap();
            previous = page.hash().unwrap();
            stream::measured(stages, "node_import_page", || {
                import.append_page(&encode(&page).unwrap(), NOW)
            });
            pages.push(page);
        }
        let witness = Signed::sign(
            EnrollmentPolicyWitness {
                version: AUTH_VERSION,
                purpose: descriptor.purpose,
                credential_hash,
                request_hash,
                snapshot_id: snapshot.payload.snapshot_id.clone(),
                snapshot_hash,
                target_heads_hash: ObjectHash::body(&descriptor.target_heads).unwrap(),
                final_page_hash: previous,
                page_count: created.bundle.journal.len() as u64,
                entry_count: created.bundle.journal.len() as u64,
                issued_at_ms: NOW as u64,
                expires_at_ms: descriptor.expires_at_ms,
            },
            &key(4),
        )
        .unwrap();
        let context = stream::measured(stages, "node_import_finish", || {
            import.finish(
                &encode(&witness).unwrap(),
                &key(node),
                &nonce(claim_nonce),
                &key(3).public_key_hex(),
                NOW,
            )
        });
        (
            context,
            serde_json::json!({"credential":credential,"request":request,"snapshot":snapshot,
                "pages":pages,"witness":witness}),
        )
    }

    fn enroll_nodes(mut created: CreatedAccount, count: u8) -> CreatedAccount {
        use node_enrollment::*;
        let backup = material(&created);
        let mut stages = Vec::new();
        for index in 0..count {
            let current = stream::confirmed_client(
                &created,
                &key(3),
                &key(4),
                NOW as u64,
                NOW as u64 + 30_000,
                &mut stages,
            );
            let node = 20 + index;
            let claim_nonce = 100 + index;
            let prepared = stream::measured(&mut stages, "node_prepare", || {
                prepare_account_node_enrollment(
                    &backup,
                    &current,
                    &key(3),
                    &claim(node, claim_nonce, NOW),
                    END,
                    NOW,
                )
            });
            let (context, _) = import_node_enrollment(
                &created,
                &prepared.request_json().unwrap(),
                node,
                claim_nonce,
                &mut stages,
            );
            let possession = stream::measured(&mut stages, "node_possession", || {
                sign_account_node_enrollment_possession(
                    &context,
                    &key(node),
                    &nonce(claim_nonce),
                    NOW,
                )
            });
            let prepared = stream::measured(&mut stages, "node_commit_prepare", || {
                prepare_account_node_enrollment_commit(&prepared, &backup, &possession, NOW)
            });
            let request: AccountNodeEnrollmentCommitRequest =
                serde_json::from_str(&prepared.request_json().unwrap()).unwrap();
            let delta = stream::measured(&mut stages, "node_authority_commit", || {
                commit_account_node_enrollment_in_state(
                    &request,
                    &created.authorization.policy_snapshot,
                    &key(4),
                    NOW as u64,
                )
            });
            let installed = stream::measured(&mut stages, "node_client_finish", || {
                finish_account_node_enrollment(&prepared, &encode(&delta).unwrap(), NOW)
            });
            let installation = stream::measured(&mut stages, "node_install", || {
                verify_account_node_installation(
                    &context,
                    &installed.installation_json().unwrap(),
                    &key(node),
                    &nonce(claim_nonce),
                    NOW,
                )
            });
            assert_eq!(installation.pins(), installed.pins());
            let (_, _, journal) = installation.into_store_parts();
            assert_eq!(journal, installed.journal());
            created.pins = installed.pins().to_vec();
            created.bundle.journal.extend(delta);
            created.authorization = installed.authorization().unwrap();
        }
        created
    }
    fn prepared(account: &CreatedAccount, node: u8) -> PreparedAccountRecovery {
        prepare_account_recovery_from_bundle(
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
        assert!(
            recovered
                .authorization
                .actions
                .contains(&Action::TaskFollowup)
        );
        let service: Signed<Grant> =
            serde_json::from_str(&recovered.service_grant_json().unwrap()).unwrap();
        assert!(!service.payload.actions.contains(&Action::TaskFollowup));
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
                prepare_account_recovery_from_bundle(
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
            prepare_account_recovery_from_bundle(
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
            prepare_account_recovery_from_bundle(
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
            prepare_account_recovery_from_bundle(
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
                prepare_account_recovery_from_bundle(
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
        use crate::signature_verification::SignatureVerification;
        use crate::webauthn::*;
        use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
        use coset::cbor::Value;
        use registration::*;
        fn registration_sample(
            intent: &Signed<AuthenticatorEnrollmentIntent>,
            nonce_byte: u8,
            credential_byte: u8,
            flags: u8,
            counter: u32,
            now: u64,
        ) -> (
            PendingPasskey,
            RegistrationResponse,
            VerifiedPasskeyRegistration,
        ) {
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
                    nonce: Nonce::from_bytes([nonce_byte; 32]),
                    not_before_ms: now,
                    expires_at_ms: now + 10_000,
                },
                RelyingParty::new("client.example", "https://client.example").unwrap(),
                &[7; 32],
                AuthenticationCeremony::IdentifiedAccount,
                vec![],
                now,
            )
            .unwrap();
            let mut auth = crate::webauthn::tests::auth(&pending, flags, counter);
            auth.extend([0; 16]);
            auth.extend(32u16.to_be_bytes());
            auth.extend([credential_byte; 32]);
            auth.extend(crate::webauthn::tests::cose());
            let object = Value::Map(vec![
                (Value::Text("fmt".into()), Value::Text("none".into())),
                (Value::Text("authData".into()), Value::Bytes(auth)),
                (Value::Text("attStmt".into()), Value::Map(vec![])),
            ]);
            let mut bytes = Vec::new();
            coset::cbor::ser::into_writer(&object, &mut bytes).unwrap();
            let response = RegistrationResponse {
                id: URL_SAFE_NO_PAD.encode([credential_byte; 32]),
                raw_id: URL_SAFE_NO_PAD.encode([credential_byte; 32]),
                credential_type: "public-key".into(),
                response: AttestationResponse {
                    client_data_json: URL_SAFE_NO_PAD
                        .encode(crate::webauthn::tests::client(&pending, "webauthn.create")),
                    attestation_object: URL_SAFE_NO_PAD.encode(bytes),
                    transports: vec![],
                },
                client_extension_results: serde_json::json!({}),
            };
            let verified = verify_passkey_registration(&pending, &response, now).unwrap();
            (pending, response, verified)
        }
        fn registration_commit(
            proposal: Signed<AccountDocument>,
            nonce_byte: u8,
            now: u64,
        ) -> Committed<AccountDocument> {
            let previous = proposal.payload.previous.clone().unwrap();
            let commit = Signed::sign(
                PolicyCommit {
                    version: AUTH_VERSION,
                    previous: previous.clone(),
                    proposal_hash: proposal.hash().unwrap(),
                    accepted_at_ms: now,
                    issued_at_ms: now,
                    expires_at_ms: now + 10_000,
                    nonce: Nonce::from_bytes([nonce_byte; 32]),
                    dependencies: vec![previous.clone()],
                },
                &key(4),
            )
            .unwrap();
            let receipt = Signed::sign(
                FreshnessReceipt {
                    version: AUTH_VERSION,
                    currents: vec![previous],
                    challenge_hash: commit.challenge_hash().unwrap(),
                    issued_at_ms: now,
                    expires_at_ms: now + 10_000,
                },
                &key(4),
            )
            .unwrap();
            Committed {
                proposal,
                commit,
                receipts: vec![receipt],
            }
        }
        let created = account_with_enrolled_nodes(12);
        assert!(created.bundle_json().unwrap().len() > MAX_PROOF_BYTES);
        let backup = material(&created);
        let mut stages = Vec::new();
        let record_stage = |stages: &mut Vec<serde_json::Value>,
                            name: &str,
                            carrier_bytes: usize,
                            signatures: &SignatureVerification,
                            usage_json: &str| {
            let usage: serde_json::Value = serde_json::from_str(usage_json).unwrap();
            assert!(carrier_bytes <= MAX_PROOF_BYTES);
            assert!(signatures.counts().actual_verifications <= MAX_PROOF_SIGNATURES);
            assert!(usage["authorization_bytes"].as_u64().unwrap() <= MAX_PROOF_BYTES as u64);
            assert!(usage["unique_signed_objects"].as_u64().unwrap() <= MAX_PROOF_OBJECTS as u64);
            assert!(usage["unique_signatures"].as_u64().unwrap() <= MAX_PROOF_SIGNATURES as u64);
            assert!(usage["max_chain_depth"].as_u64().unwrap() <= MAX_DELEGATION_DEPTH as u64);
            stages.push(
                serde_json::json!({"stage":name,"carrier_bytes":carrier_bytes,
                "usage":usage,"actual":signatures.counts()}),
            );
        };
        let (current, client_policy) = stream::confirmed_client_with_evidence(
            &created,
            &key(3),
            &key(4),
            NOW as u64,
            NOW as u64 + 30_000,
            &mut stages,
        );
        assert!(client_policy["pages"].as_array().unwrap().len() >= 34);
        let build_signatures = SignatureVerification::new();
        let signed_intent = build_signatures
            .run(|| {
                build_first_authenticator_enrollment_intent(&backup, &current, &key(3), NOW, END)
            })
            .unwrap();
        let intent: Signed<AuthenticatorEnrollmentIntent> =
            serde_json::from_str(&signed_intent).unwrap();
        let mut build_usage = recovery_material_usage(&read_material(&backup).unwrap()).unwrap();
        build_usage.record_budget(&current.budget).unwrap();
        let grant_depth = current
            .policy_snapshot
            .grant_chain(Some(&current.grant_leaf))
            .unwrap()
            .len();
        assert!(grant_depth <= MAX_DELEGATION_DEPTH);
        build_usage.record_chain_depth(grant_depth);
        build_usage.record_signed(&intent).unwrap();
        build_usage.record_actual_signature_counts(build_signatures.counts());
        record_stage(
            &mut stages,
            "first_authenticator_build",
            backup.len() + signed_intent.len(),
            &build_signatures,
            &encode(&build_usage.counts()).unwrap(),
        );
        assert!(
            build_first_authenticator_enrollment_intent(&backup, &current, &key(4), NOW, END)
                .is_err()
        );
        assert!(
            build_first_authenticator_enrollment_intent(
                &backup,
                &created.authorization,
                &key(3),
                NOW,
                END
            )
            .is_err()
        );
        let other_prepared =
            prepare_account_creation(&key(70), &key(71), &key(72), &claim(73, 74, NOW), END, NOW)
                .unwrap();
        let other_possession = sign_node_possession(
            &other_prepared.node_request_json().unwrap(),
            &key(73),
            &nonce(74),
            NOW,
        )
        .unwrap();
        let other =
            finish_account_creation(&other_prepared, &key(70), &other_possession, NOW).unwrap();
        let other_current = stream::confirmed_client(
            &other,
            &key(72),
            &key(73),
            NOW as u64,
            NOW as u64 + 30_000,
            &mut stages,
        );
        assert!(
            build_first_authenticator_enrollment_intent(
                &backup,
                &other_current,
                &key(72),
                NOW,
                END,
            )
            .is_err()
        );
        let (pending, response, verified) =
            registration_sample(&intent, 96, 9, 0x5d, 7, NOW as u64);
        for unapproved in [&created.authorization, &other_current] {
            assert!(
                prepare_first_authenticator_registration(
                    &backup,
                    unapproved,
                    &signed_intent,
                    &verified,
                    NOW,
                )
                .is_err()
            );
        }
        let prepare_signatures = SignatureVerification::new();
        let prepared = prepare_signatures
            .run(|| {
                prepare_first_authenticator_registration(
                    &backup,
                    &current,
                    &signed_intent,
                    &verified,
                    NOW,
                )
            })
            .unwrap();
        let request = prepared.request_json().unwrap();
        let prepare_usage: serde_json::Value =
            serde_json::from_str(&prepared.proof_usage_json().unwrap()).unwrap();
        assert!(prepare_usage["max_chain_depth"].as_u64().unwrap() >= grant_depth as u64);
        record_stage(
            &mut stages,
            "first_authenticator_prepare",
            backup.len() + signed_intent.len() + request.len(),
            &prepare_signatures,
            &prepared.proof_usage_json().unwrap(),
        );
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
        let mut committed = registration_commit(proposal, 97, NOW as u64);
        let delta = vec![
            JournalEntry::Authenticator(record.clone()),
            JournalEntry::AccountCommit(committed.clone()),
        ];
        let delta_json = serde_json::to_string(&delta).unwrap();
        let finish_signatures = SignatureVerification::new();
        let finished = finish_signatures
            .run(|| finish_first_authenticator_registration(&prepared, &delta_json, NOW))
            .unwrap();
        record_stage(
            &mut stages,
            "first_authenticator_finish",
            delta_json.len(),
            &finish_signatures,
            &finished.proof_usage_json().unwrap(),
        );
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
        let snapshot = finished.snapshot();
        let updated_bundle = ClientAuthorizationBundle {
            journal: snapshot.journal().to_vec(),
            grant_leaf: created.bundle.grant_leaf.clone(),
        };
        assert_eq!(snapshot.pins(), updated_pins);
        assert_eq!(
            &updated_bundle.journal[created.bundle.journal.len()..],
            delta
        );
        let state = snapshot.state();
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
        let updated = CreatedAccount {
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
        };
        let updated_current = stream::confirmed_client(
            &updated,
            &key(3),
            &key(4),
            NOW as u64,
            NOW as u64 + 30_000,
            &mut stages,
        );
        assert!(
            build_first_authenticator_enrollment_intent(
                &backup,
                &updated_current,
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
        let registered = updated;
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
        let removed_current = stream::confirmed_client(
            &removed,
            &key(3),
            &key(4),
            NOW as u64,
            NOW as u64 + 30_000,
            &mut stages,
        );
        assert!(
            build_first_authenticator_enrollment_intent(
                &removed_material,
                &removed_current,
                &key(3),
                NOW,
                END,
            )
            .is_err()
        );
        let mut new_stages = Vec::new();
        let (recovery_context, recovery_pages) = stream::import_account_history(
            &removed,
            &removed_material,
            &key(50),
            RESTORE,
            RESTORE + 30_000.0,
            86,
            &mut new_stages,
        );
        assert!(
            recovery_context
                .policy()
                .has_registered_authenticator(&intent.payload.account, 0)
        );
        let recovery_prepared = stream::measured(&mut new_stages, "recovery_prepare", || {
            prepare_account_recovery(
                &removed_material,
                &recovery_context,
                &key(51),
                &key(50),
                END,
                RESTORE,
            )
        });
        let recovery_approved: stream::StreamedRecoveryNodeRequest =
            serde_json::from_str(&recovery_prepared.node_request_json().unwrap()).unwrap();
        let recovery_prefix = ObjectHash::body(&(
            "fixed-native-accepted-prefix",
            removed.bundle.journal.len(),
            ObjectHash::body(removed.bundle.journal.last().unwrap()).unwrap(),
        ))
        .unwrap();
        let (recovery_possession, recovery_approval) =
            stream::measured(&mut new_stages, "recovery_possession", || {
                stream::approve_recovery_source_proposal(
                    &recovery_approved,
                    recovery_context.request(),
                    recovery_context.snapshot(),
                    recovery_context.witness(),
                    &removed.authorization.policy_snapshot,
                    &key(4),
                    recovery_context.request().nonce(),
                    &recovery_prefix,
                    RESTORE as u64,
                )
            });
        let new_generation = stream::measured(&mut new_stages, "recovery_finish", || {
            finish_account_recovery(
                &recovery_prepared,
                &key(51),
                recovery_possession.as_str(),
                RESTORE,
            )
        });
        let recovery_commit: stream::StreamedRecoveryCommitRequest =
            serde_json::from_str(&recovery_prepared.commit_json(&new_generation).unwrap()).unwrap();
        let (recovery_committed, approval) =
            stream::measured(&mut new_stages, "recovery_commit", || {
                let approval = stream::verify_recovery_source_proposal_approval(
                    &recovery_approval,
                    recovery_context.request(),
                    &recovery_approved.context,
                    &ObjectHash::body(&recovery_approved)?,
                    &recovery_approved.request.account_recovery,
                    &recovery_possession,
                    &removed.authorization.policy_snapshot,
                    &PublicKey::of(&key(4)),
                    &recovery_prefix,
                    RESTORE as u64,
                )?;
                let committed = stream::verify_recovery_stream_commit_with_source_approval(
                    &recovery_commit,
                    &recovery_approved,
                    recovery_context.request(),
                    &removed.authorization.policy_snapshot,
                    &approval,
                    RESTORE as u64,
                )?;
                Ok::<_, AuthorizationError>((committed, approval))
            });
        let mut old_template = recovery_commit.clone();
        let owner = old_template
            .journal_delta
            .iter_mut()
            .find_map(|entry| match entry {
                JournalEntry::Grant(grant) if grant.payload.parent.is_none() => Some(grant),
                _ => None,
            })
            .unwrap();
        assert!(owner.payload.actions.contains(&Action::TaskFollowup));
        let mut payload = owner.payload.clone();
        payload
            .actions
            .retain(|action| *action != Action::TaskFollowup);
        *owner = Signed::sign(payload, &key(51)).unwrap();
        assert_eq!(
            stream::verify_recovery_stream_commit_with_source_approval(
                &old_template,
                &recovery_approved,
                recovery_context.request(),
                &removed.authorization.policy_snapshot,
                &approval,
                RESTORE as u64,
            )
            .err(),
            Some(AuthorizationError::InvalidProof(
                "recovery changed its exact owner grant"
            ))
        );
        let recovery_usage = recovery_committed.proof_usage();
        assert!(recovery_usage.carrier_bytes <= MAX_PROOF_BYTES);
        assert!(recovery_usage.proof.unique_signed_objects <= MAX_PROOF_OBJECTS);
        assert!(recovery_usage.proof.unique_signatures <= MAX_PROOF_SIGNATURES);
        assert!(recovery_usage.proof.max_chain_depth <= MAX_DELEGATION_DEPTH);
        new_stages.last_mut().unwrap()["usage"] = serde_json::to_value(recovery_usage).unwrap();
        assert_eq!(
            recovery_committed.committed_heads(),
            new_generation
                .pins
                .iter()
                .filter_map(|pin| pin.minimum_head.clone())
                .collect::<Vec<_>>()
        );
        assert_eq!(
            recovery_committed.authorization().scope,
            new_generation.authorization.scope
        );
        let new_material = stream::measured(&mut new_stages, "recovery_backup", || {
            build_recovered_account_recovery_material(
                &new_generation,
                &key(51),
                &removed_material,
                ENDPOINT,
                RESTORE,
            )
        });
        assert!(new_material.len() <= MAX_RECOVERY_MATERIAL_BYTES);
        let new_backup_usage =
            recovery_material_usage(&read_material(&new_material).unwrap()).unwrap();
        new_stages.last_mut().unwrap()["usage"] =
            serde_json::to_value(new_backup_usage.counts()).unwrap();
        let (new_current, new_client_policy) = stream::confirmed_client_with_evidence(
            &new_generation,
            &key(50),
            &key(4),
            RESTORE as u64,
            RESTORE as u64 + 30_000,
            &mut new_stages,
        );
        let new_snapshot =
            crate::authorization::client_policy_import::require_confirmed_client_policy(
                &new_current,
            )
            .unwrap();
        let new_account = new_snapshot
            .state()
            .account(&intent.payload.account)
            .unwrap();
        assert_eq!(new_account.root_generation, 1);
        assert_eq!(new_account.current_root, PublicKey::of(&key(51)));
        assert!(new_account.authenticators.is_empty());
        assert!(new_snapshot.has_registered_authenticator(&intent.payload.account, 0));
        assert!(!new_snapshot.has_registered_authenticator(&intent.payload.account, 1));
        let new_signed_intent =
            stream::measured(&mut new_stages, "first_authenticator_build", || {
                build_first_authenticator_enrollment_intent(
                    &new_material,
                    &new_current,
                    &key(50),
                    RESTORE,
                    END,
                )
            });
        let new_intent: Signed<AuthenticatorEnrollmentIntent> =
            serde_json::from_str(&new_signed_intent).unwrap();
        assert_eq!(new_intent.issuer, PublicKey::of(&key(51)));
        assert_eq!(new_intent.payload.account, intent.payload.account);
        let (new_pending, new_response, new_verified) =
            registration_sample(&new_intent, 98, 10, 0x4d, 11, RESTORE as u64);
        let new_prepare_signatures = SignatureVerification::new();
        let new_prepared = new_prepare_signatures
            .run(|| {
                prepare_first_authenticator_registration(
                    &new_material,
                    &new_current,
                    &new_signed_intent,
                    &new_verified,
                    RESTORE,
                )
            })
            .unwrap();
        let new_request = new_prepared.request_json().unwrap();
        record_stage(
            &mut new_stages,
            "first_authenticator_prepare",
            new_material.len() + new_signed_intent.len() + new_request.len(),
            &new_prepare_signatures,
            &new_prepared.proof_usage_json().unwrap(),
        );
        let new_wire: serde_json::Value = serde_json::from_str(&new_request).unwrap();
        let new_record: Signed<AuthenticatorRecord> =
            serde_json::from_value(new_wire["record"].clone()).unwrap();
        assert_ne!(
            new_record.payload.credential_id,
            record.payload.credential_id
        );
        assert_eq!(new_record.payload.nonce, new_pending.envelope().nonce);
        assert_eq!(
            new_record.payload.initial_state,
            AuthenticatorState {
                sign_count: 11,
                backup_state: false,
            }
        );
        let new_proposal: Signed<AccountDocument> =
            serde_json::from_value(new_wire["proposal"].clone()).unwrap();
        let new_delta = vec![
            JournalEntry::Authenticator(new_record.clone()),
            JournalEntry::AccountCommit(registration_commit(new_proposal, 99, RESTORE as u64)),
        ];
        let new_delta_json = serde_json::to_string(&new_delta).unwrap();
        let new_finish_signatures = SignatureVerification::new();
        let new_finished = new_finish_signatures
            .run(|| {
                finish_first_authenticator_registration(&new_prepared, &new_delta_json, RESTORE)
            })
            .unwrap();
        record_stage(
            &mut new_stages,
            "first_authenticator_finish",
            new_delta_json.len(),
            &new_finish_signatures,
            &new_finished.proof_usage_json().unwrap(),
        );
        let final_snapshot = new_finished.snapshot();
        assert!(final_snapshot.has_registered_authenticator(&intent.payload.account, 0));
        assert!(final_snapshot.has_registered_authenticator(&intent.payload.account, 1));
        let final_account = final_snapshot
            .state()
            .account(&intent.payload.account)
            .unwrap();
        assert_eq!(final_account.root_generation, 1);
        assert_eq!(final_account.current_root, PublicKey::of(&key(51)));
        assert_eq!(
            final_account.authenticators,
            vec![new_record.hash().unwrap()]
        );
        assert_eq!(
            final_snapshot
                .state()
                .verify_authenticator(&new_record, RESTORE as u64)
                .unwrap()
                .state_floor(),
            new_record.payload.initial_state
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
            let value = serde_json::json!({"now":NOW,"end":END,"material":backup,
                "client_policy":client_policy,"intent":intent,"pending":pending,
                "response":response,"request":wire,"committed_journal":delta,
                "updated_pins":updated_pins,"stages":stages,
                "new_generation":{"now":RESTORE,"end":END,"previous_material":removed_material,
                    "recovery":{"request":match recovery_context.request() { stream::RecoveryReadRequest::Account(request) => request, _ => unreachable!() },
                        "snapshot":recovery_context.snapshot(),"pages":recovery_pages,"witness":recovery_context.witness(),
                        "node_request":recovery_approved,"node_possession":recovery_possession,
                        "source_approval":recovery_approval,"prefix_commitment":recovery_prefix,"commit":recovery_commit},
                    "material":new_material,"client_policy":new_client_policy,"intent":new_intent,
                    "pending":new_pending,"response":new_response,"request":new_wire,
                    "committed_journal":new_delta,"updated_pins":final_snapshot.pins(),"stages":new_stages}});
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

    #[test]
    fn additional_node_keeps_the_account_and_needs_root_possession_and_authority() {
        use node_enrollment::*;
        let original = account();
        let backup = material(&original);
        let mut stages = Vec::new();
        let (current, client_policy) = stream::confirmed_client_with_evidence(
            &original,
            &key(3),
            &key(4),
            NOW as u64,
            NOW as u64 + 30_000,
            &mut stages,
        );
        let new_claim = claim(7, 44, NOW);
        let prepared = stream::measured(&mut stages, "node_prepare", || {
            prepare_account_node_enrollment(&backup, &current, &key(3), &new_claim, END, NOW)
        });
        let request = prepared.request_json().unwrap();
        assert!(!request.contains("secret_hex"));
        let envelope: serde_json::Value = serde_json::from_str(&request).unwrap();
        let credential: AccountNodeEnrollmentCredential =
            serde_json::from_value(envelope["credential"]["credential"].clone()).unwrap();
        let signed = credential.intent.clone();
        assert_eq!(
            signed.payload.certificate.owner,
            PolicyDomain::Account(original.authorization.scope.account_id.clone())
        );
        assert_eq!(
            signed.payload.certificate.uses,
            [DeviceUse::Noise, DeviceUse::ServiceInvocation]
        );
        let (context, policy_import) =
            import_node_enrollment(&original, &request, 7, 44, &mut stages);
        assert!(
            sign_account_node_enrollment_possession(&context, &key(8), &nonce(44), NOW).is_err()
        );
        assert!(
            sign_account_node_enrollment_possession(&context, &key(7), &nonce(43), NOW).is_err()
        );
        assert!(
            sign_account_node_enrollment_possession(&context, &key(7), &nonce(44), NOW + 100_000.0)
                .is_err()
        );
        let possession = stream::measured(&mut stages, "node_possession", || {
            sign_account_node_enrollment_possession(&context, &key(7), &nonce(44), NOW)
        });
        assert!(
            prepare_account_node_enrollment_commit(&prepared, &backup, &"00".repeat(64), NOW)
                .is_err()
        );
        let commit = stream::measured(&mut stages, "node_commit_prepare", || {
            prepare_account_node_enrollment_commit(&prepared, &backup, &possession, NOW)
        });
        let public = commit.request_json().unwrap();
        let commit_request: AccountNodeEnrollmentCommitRequest =
            serde_json::from_str(&public).unwrap();
        assert!(
            commit_account_node_enrollment_in_state(
                &commit_request,
                &original.authorization.policy_snapshot,
                &key(1),
                NOW as u64,
            )
            .is_err()
        );
        let delta = stream::measured(&mut stages, "node_authority_commit", || {
            commit_account_node_enrollment_in_state(
                &commit_request,
                &original.authorization.policy_snapshot,
                &key(4),
                NOW as u64,
            )
        });
        assert!(matches!(
            delta.as_slice(),
            [JournalEntry::Device(_), JournalEntry::AccountCommit(_)]
        ));
        let delta_json = encode(&delta).unwrap();
        let result = stream::measured(&mut stages, "node_client_finish", || {
            finish_account_node_enrollment(&commit, &delta_json, NOW)
        });
        let delayed =
            finish_account_node_enrollment(&commit, &delta_json, NOW + 200_000.0).unwrap();
        assert_eq!(delayed.pins_json().unwrap(), result.pins_json().unwrap());
        assert_eq!(result.account(), &original.authorization.scope.account_id);
        assert_eq!(
            result.authorization().unwrap().grant_leaf,
            original.bundle.grant_leaf
        );
        let expired_proof =
            finish_account_node_enrollment(&commit, &delta_json, END + 1.0).unwrap();
        assert_eq!(
            expired_proof.pins_json().unwrap(),
            result.pins_json().unwrap()
        );
        assert!(
            verify_account_node_installation(
                &context,
                &expired_proof.installation_json().unwrap(),
                &key(7),
                &nonce(44),
                END + 1.0
            )
            .is_err()
        );
        let installation = result.installation_json().unwrap();
        assert!(!installation.contains("secret_hex"));
        let installed = stream::measured(&mut stages, "node_install", || {
            verify_account_node_installation(
                &context,
                &installation,
                &key(7),
                &nonce(44),
                NOW + 200_000.0,
            )
        });
        assert_eq!(installed.account(), result.account());
        assert_eq!(installed.node_public_key(), &PublicKey::of(&key(7)));
        let (after, installed_pins, installed_journal) = installed.into_store_parts();
        assert_eq!(installed_pins, result.pins());
        assert_eq!(installed_journal, result.journal());
        let before = &original.authorization.policy_snapshot;
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
        assert!(
            verify_account_node_installation(&context, &installation, &key(8), &nonce(44), NOW)
                .is_err()
        );
        assert!(
            verify_account_node_installation(&context, &installation, &key(7), &nonce(43), NOW)
                .is_err()
        );
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
        let wrong = AccountNodeEnrollmentCredential {
            intent: Signed::sign(wrong, &key(1)).unwrap(),
            ..credential.clone()
        };
        assert!(
            verify_account_node_enrollment_read(
                before,
                &wrong.intent,
                &wrong.descriptor,
                &PublicKey::of(&key(4)),
                &PublicKey::of(&key(3)),
                NOW as u64,
            )
            .is_err()
        );
        let mut changed_context = credential;
        changed_context.descriptor.pins.clear();
        assert!(
            begin_account_node_enrollment_import(
                &encode(&changed_context).unwrap(),
                &encode(&policy_import["request"]).unwrap(),
                &encode(&policy_import["snapshot"]).unwrap(),
                &key(7),
                &nonce(44),
                &key(3).public_key_hex(),
                NOW
            )
            .is_err()
        );
        let mut changed_installation: serde_json::Value =
            serde_json::from_str(&installation).unwrap();
        changed_installation["request"]["descriptor"]["pins"] = serde_json::json!([]);
        assert!(
            verify_account_node_installation(
                &context,
                &encode(&changed_installation).unwrap(),
                &key(7),
                &nonce(44),
                NOW
            )
            .err()
            .unwrap()
            .contains("context")
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
                    &current,
                    &key(3),
                    &claim(reused, 44, NOW),
                    END,
                    NOW
                )
                .is_err()
            );
        }
        if let Ok(path) = std::env::var("VIBEFACE_NODE_ENROLLMENT_VECTOR") {
            let value = serde_json::json!({"now":NOW,"end":END,"material":backup,
                "pins":original.pins,"claim":new_claim.claim,"client_policy":client_policy,
                "request":envelope,"policy_import":policy_import,"possession":possession,
                "commit_request":commit_request,"delta":delta,
                "installation":serde_json::from_str::<serde_json::Value>(&installation).unwrap(),
                "updated_pins":result.pins(),"stages":stages});
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
