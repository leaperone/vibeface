//! Root and recovery approved state for a private recovery backup, never a sync anchor.

use super::*;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    content = "value",
    rename_all = "snake_case",
    deny_unknown_fields
)]
enum PolicyWitness {
    Account(Signed<AccountDocument>),
    Space(Signed<SpaceDocument>),
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RecoveryPolicySnapshot {
    version: u16,
    scope: ClientScope,
    pins: Vec<PolicyPin>,
    entry_public_key: PublicKey,
    bundle_hash: ObjectHash,
    account_genesis: Signed<AccountDocument>,
    space_genesis: Signed<SpaceDocument>,
    authority_changes: Vec<Signed<AccountDocument>>,
    account: Signed<AccountDocument>,
    space: Signed<SpaceDocument>,
    policy_witnesses: Vec<PolicyWitness>,
    authenticator_states: Vec<(ObjectHash, crate::webauthn::AuthenticatorState)>,
}
impl sealed::Sealed for RecoveryPolicySnapshot {}
impl SignedObject for RecoveryPolicySnapshot {
    const DOMAIN: &'static str = "private-recovery-policy-snapshot-v2";
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecoveryPolicyCheckpoint {
    snapshot: Signed<RecoveryPolicySnapshot>,
    recovery_approval: KeyApproval,
}
const APPROVAL_DOMAIN: &str = "private-recovery-policy-snapshot-approval-v2";

pub(super) fn scope_budget(
    state: &VerifiedPolicySet,
    material: &RecoveryMaterial,
    now: u64,
    budget: &mut Budget,
) -> Result<()> {
    let certificate = state.find_device(&material.scope.device_id)?;
    require(
        certificate.payload.subject == Actor::Account(material.scope.account_id.clone()),
        "recovery client belongs to another account",
    )?;
    let mut dependencies = BTreeMap::new();
    state.validate_certificate(
        certificate,
        now,
        budget,
        &mut dependencies,
        &mut BTreeSet::new(),
    )?;
    let path = state.grant_chain(Some(&material.bundle.grant_leaf))?;
    state.verify_grant_path(&path, now, budget, &mut dependencies, true)?;
    let leaf = &path.last().ok_or(AuthorizationError::Revoked)?.payload;
    require(
        leaf.owner_space == material.scope.space_id
            && state.matches_grantee(
                &leaf.grantee,
                &Actor::Account(material.scope.account_id.clone()),
                now,
                budget,
                &mut dependencies,
            )?,
        "recovery backup authorization mismatch",
    )?;
    let target = state.find_device(&leaf.audience.device)?;
    require(
        target.payload.subject == Actor::Service(leaf.audience.service.clone())
            && target.payload.public_key == material.entry.node_public_key,
        "recovery entry pin mismatch",
    )?;
    state.validate_service_reference(&material.scope.space_id, target)?;
    state.validate_certificate(target, now, budget, &mut dependencies, &mut BTreeSet::new())?;
    let space = state
        .space(&material.scope.space_id)
        .ok_or(AuthorizationError::Revoked)?;
    state.ensure_personal_current(space)?;
    if let ResourceScope::Exact(resource) = &leaf.scope {
        let record = state.resource(&resource.space, &resource.resource).ok_or(
            AuthorizationError::InvalidProof("missing exact recovery resource"),
        )?;
        require(
            record.payload.host == leaf.audience.service,
            "recovery resource audience mismatch",
        )?;
        budget.verify(record)?;
    }
    for hash in &space.resources {
        let Some(record) = state.resources.get(hash) else {
            continue;
        };
        let resource = ResourceRef {
            space: record.payload.space.clone(),
            resource: record.payload.resource.clone(),
        };
        if scope_covers(&leaf.scope, &resource)
            && record.payload.host == leaf.audience.service
            && record.payload.kind == ResourceKind::Project
        {
            budget.verify(record)?;
        }
    }
    state.expand_authorities(now, budget, &mut dependencies)
}

fn object_stamp(entry: &JournalEntry) -> Result<PolicyStamp> {
    Ok(match entry {
        JournalEntry::Device(value) => value.payload.issued_under.clone(),
        JournalEntry::Resource(value) => value.payload.issued_under.clone(),
        JournalEntry::Grant(value) => value.payload.issued_under.clone(),
        JournalEntry::Authenticator(value) => value.payload.issued_under.clone(),
        _ => {
            return Err(AuthorizationError::InvalidProof(
                "private checkpoint requires only scoped objects",
            ));
        }
    })
}

fn object_hash(entry: &JournalEntry) -> Result<ObjectHash> {
    match entry {
        JournalEntry::Device(value) => value.hash(),
        JournalEntry::Resource(value) => value.hash(),
        JournalEntry::Grant(value) => value.hash(),
        JournalEntry::Authenticator(value) => value.hash(),
        _ => Err(AuthorizationError::InvalidProof(
            "invalid private checkpoint object",
        )),
    }
}

pub(crate) fn compact_recovery_material(material: &mut RecoveryMaterial, now: u64) -> Result<()> {
    if zeroize::Zeroizing::new(canonical(material)?).len() <= MAX_RECOVERY_MATERIAL_BYTES {
        return Ok(());
    }
    let state = replay_policies(&material.pins, &material.bundle.journal)?;
    compact_recovery_material_in_state(material, &state, now)
}

pub(crate) fn compact_recovery_material_in_state(
    material: &mut RecoveryMaterial,
    state: &VerifiedPolicySet,
    now: u64,
) -> Result<()> {
    if material.policy_checkpoint.is_some() {
        material_state(material)?;
        return require(
            zeroize::Zeroizing::new(canonical(material)?).len() <= MAX_RECOVERY_MATERIAL_BYTES,
            "private recovery checkpoint exceeds 64 KiB",
        );
    }
    let mut history_usage = crate::authorization::execution_handoff::ExecutionProofUsage::default();
    let history_bounded = material.bundle.journal.iter().all(|entry| {
        match crate::authorization::policy_proof::policy_entry_payload_usage(entry) {
            Ok(usage) => {
                history_usage.merge(&usage);
                history_usage.enforce_limits().is_ok()
            }
            Err(_) => false,
        }
    });
    if history_bounded
        && zeroize::Zeroizing::new(canonical(material)?).len() <= MAX_RECOVERY_MATERIAL_BYTES
    {
        return Ok(());
    }
    checkpoint_recovery_material_in_state(material, state, now)
}

pub(crate) fn checkpoint_recovery_material_in_state(
    material: &mut RecoveryMaterial,
    state: &VerifiedPolicySet,
    now: u64,
) -> Result<()> {
    let root = Identity::from_secret_hex(&material.root_secret_hex)
        .map_err(|_| AuthorizationError::InvalidProof("invalid checkpoint root"))?;
    let recovery = Identity::from_secret_hex(&material.recovery_secret_hex)
        .map_err(|_| AuthorizationError::InvalidProof("invalid checkpoint recovery key"))?;
    let accounts = state
        .account_history(&material.scope.account_id)
        .ok_or(AuthorizationError::Revoked)?;
    let spaces = state
        .space_history(&material.scope.space_id)
        .ok_or(AuthorizationError::Revoked)?;
    let account = accounts.last().ok_or(AuthorizationError::Revoked)?.clone();
    let space = spaces.last().ok_or(AuthorizationError::Revoked)?.clone();
    require(
        account.payload.current_root == PublicKey::of(&root)
            && account.payload.recovery.threshold == 1
            && account.payload.recovery.keys == [PublicKey::of(&recovery)]
            && material.pins.len() == 2
            && material
                .pins
                .windows(2)
                .all(|pins| pins[0].domain < pins[1].domain)
            && material.pins.iter().all(|pin| {
                (pin.domain == PolicyDomain::Account(material.scope.account_id.clone())
                    || pin.domain == PolicyDomain::Space(material.scope.space_id.clone()))
                    && pin.genesis_root == account.payload.genesis_root
                    && pin.minimum_head.as_ref() == state.head(&pin.domain)
            }),
        "private checkpoint needs current Root and RecoveryKey approval",
    )?;
    let mut budget = Budget::default();
    scope_budget(&state, material, now, &mut budget)?;
    let mut selected = budget.objects;
    selected.extend(account.payload.authenticators.iter().cloned());
    let mut objects = Vec::new();
    for (hash, value) in &state.devices {
        if selected.contains(hash) {
            objects.push(JournalEntry::Device(value.clone()));
        }
    }
    for (hash, value) in &state.resources {
        if selected.contains(hash) {
            objects.push(JournalEntry::Resource(value.clone()));
        }
    }
    for (hash, value) in &state.authenticators {
        if selected.contains(hash) {
            objects.push(JournalEntry::Authenticator(value.clone()));
        }
    }
    for (hash, value) in &state.grants {
        if selected.contains(hash) {
            objects.push(JournalEntry::Grant(value.clone()));
        }
    }
    let bundle = ClientAuthorizationBundle {
        journal: objects,
        grant_leaf: material.bundle.grant_leaf.clone(),
    };
    let mut stamps = BTreeSet::new();
    for entry in &bundle.journal {
        stamps.insert(object_stamp(entry)?);
    }
    let mut policy_witnesses = Vec::new();
    for stamp in stamps {
        let witness = match &stamp.domain {
            PolicyDomain::Account(id) if id == &material.scope.account_id => accounts
                .iter()
                .find(|value| account_stamp(value).as_ref() == Ok(&stamp))
                .cloned()
                .map(PolicyWitness::Account),
            PolicyDomain::Space(id) if id == &material.scope.space_id => spaces
                .iter()
                .find(|value| space_stamp(value).as_ref() == Ok(&stamp))
                .cloned()
                .map(PolicyWitness::Space),
            _ => None,
        }
        .ok_or(AuthorizationError::InvalidProof(
            "missing original checkpoint policy witness",
        ))?;
        policy_witnesses.push(witness);
    }
    if let Some(registered) = accounts.iter().find(|value| {
        value.payload.root_generation == account.payload.root_generation
            && !value.payload.authenticators.is_empty()
    }) {
        let stamp = account_stamp(registered)?;
        if !policy_witnesses.iter().any(|witness| {
            matches!(witness, PolicyWitness::Account(value) if account_stamp(value).as_ref() == Ok(&stamp))
        }) {
            policy_witnesses.push(PolicyWitness::Account(registered.clone()));
        }
    }
    let mut authority_changes = Vec::new();
    let mut previous = &accounts[0].payload;
    for value in &accounts[1..] {
        let next = &value.payload;
        if next.current_root != previous.current_root
            || next.recovery != previous.recovery
            || next.authority_epoch != previous.authority_epoch
            || next.authority != previous.authority
        {
            authority_changes.push(value.clone());
        }
        previous = next;
    }
    let mut authenticator_states = Vec::new();
    for hash in &account.payload.authenticators {
        let value = state
            .authenticator_states
            .get(hash)
            .copied()
            .or_else(|| {
                state
                    .authenticators
                    .get(hash)
                    .map(|record| record.payload.initial_state)
            })
            .ok_or(AuthorizationError::InvalidProof(
                "missing current authenticator floor",
            ))?;
        authenticator_states.push((hash.clone(), value));
    }
    let snapshot = Signed::sign(
        RecoveryPolicySnapshot {
            version: AUTH_VERSION,
            scope: material.scope.clone(),
            pins: material.pins.clone(),
            entry_public_key: material.entry.node_public_key.clone(),
            bundle_hash: ObjectHash::body(&bundle)?,
            account_genesis: accounts[0].clone(),
            space_genesis: spaces[0].clone(),
            authority_changes,
            account,
            space,
            policy_witnesses,
            authenticator_states,
        },
        &root,
    )?;
    let recovery_approval = KeyApproval {
        key: PublicKey::of(&recovery),
        signature: Signature::parse(
            &recovery
                .sign(APPROVAL_DOMAIN, &canonical(&snapshot)?)
                .map_err(|_| {
                    AuthorizationError::InvalidProof("checkpoint recovery approval failed")
                })?,
        )?,
    };
    let old_bundle = std::mem::replace(&mut material.bundle, bundle);
    let old_checkpoint = material
        .policy_checkpoint
        .replace(RecoveryPolicyCheckpoint {
            snapshot,
            recovery_approval,
        });
    let validate = recovery_material_usage(material).and_then(|_| {
        require(
            zeroize::Zeroizing::new(canonical(material)?).len() <= MAX_RECOVERY_MATERIAL_BYTES,
            "private recovery checkpoint exceeds 64 KiB",
        )
    });
    if let Err(error) = validate {
        material.policy_checkpoint = old_checkpoint;
        material.bundle = old_bundle;
        return Err(error);
    }
    Ok(())
}

pub(crate) fn material_state(material: &RecoveryMaterial) -> Result<VerifiedPolicySet> {
    if material.policy_checkpoint.is_none() {
        return replay_policies(&material.pins, &material.bundle.journal);
    }
    let _verification = crate::signature_verification::ScopedSignatureVerification::new();
    material_state_with_budget(material, &mut Budget::default())
}

pub(super) fn material_proof_usage(
    material: &RecoveryMaterial,
) -> Result<crate::authorization::execution_handoff::ExecutionProofUsage> {
    let mut usage = crate::authorization::execution_handoff::ExecutionProofUsage::default();
    let mut budget = Budget::default();
    let state = material_state_with_budget(material, &mut budget)?;
    if material.policy_checkpoint.is_none() {
        for entry in &material.bundle.journal {
            usage.merge(&crate::authorization::policy_proof::policy_entry_payload_usage(entry)?);
        }
        let at = state
            .find_device(&material.scope.device_id)?
            .payload
            .not_before_ms;
        scope_budget(&state, material, at, &mut budget)?;
    }
    usage.record_budget(&budget)?;
    usage.enforce_limits()?;
    Ok(usage)
}

/// Counts the private backup produced from genuine state without revalidating it.
pub(crate) fn recovery_material_usage(
    material: &RecoveryMaterial,
) -> Result<crate::authorization::execution_handoff::ExecutionProofUsage> {
    use crate::authorization::policy_proof::policy_entry_payload_usage;
    let mut usage = crate::authorization::execution_handoff::ExecutionProofUsage::default();
    usage.record_metadata(&(&material.scope, &material.pins, &material.entry))?;
    if let Some(checkpoint) = &material.policy_checkpoint {
        usage.record_signed(&checkpoint.snapshot)?;
        let value = &checkpoint.snapshot.payload;
        let mut raw = Budget::default();
        raw.signatures.insert((
            checkpoint.recovery_approval.key.clone(),
            APPROVAL_DOMAIN.to_owned(),
            ObjectHash(format!(
                "h2_{}",
                digest(APPROVAL_DOMAIN, &canonical(&checkpoint.snapshot)?)
            )),
            checkpoint.recovery_approval.signature.clone(),
        ));
        usage.record_budget(&raw)?;
        for account in [&value.account_genesis, &value.account]
            .into_iter()
            .chain(value.authority_changes.iter())
        {
            usage.merge(&policy_entry_payload_usage(&JournalEntry::Account(
                account.clone(),
            ))?);
        }
        for space in [&value.space_genesis, &value.space] {
            usage.merge(&policy_entry_payload_usage(&JournalEntry::Space(
                space.clone(),
            ))?);
        }
        for witness in &value.policy_witnesses {
            let entry = match witness {
                PolicyWitness::Account(value) => JournalEntry::Account(value.clone()),
                PolicyWitness::Space(value) => JournalEntry::Space(value.clone()),
            };
            usage.merge(&policy_entry_payload_usage(&entry)?);
        }
    }
    for entry in &material.bundle.journal {
        usage.merge(&policy_entry_payload_usage(entry)?);
    }
    usage.enforce_limits()?;
    Ok(usage)
}

fn material_state_with_budget(
    material: &RecoveryMaterial,
    budget: &mut Budget,
) -> Result<VerifiedPolicySet> {
    let Some(checkpoint) = &material.policy_checkpoint else {
        return replay_policies(&material.pins, &material.bundle.journal);
    };
    let value = &checkpoint.snapshot.payload;
    require(
        value.version == AUTH_VERSION
            && value.scope == material.scope
            && value.pins == material.pins
            && value.pins.len() == 2
            && value
                .pins
                .windows(2)
                .all(|pins| pins[0].domain < pins[1].domain)
            && value.bundle_hash == ObjectHash::body(&material.bundle)?
            && value.entry_public_key == material.entry.node_public_key
            && material.scope.space_id == SpaceId::personal(&material.scope.account_id)
            && value.account_genesis.payload.account == material.scope.account_id
            && value.space_genesis.payload.space == material.scope.space_id
            && value.authority_changes.len() <= MAX_PROOF_OBJECTS
            && value.policy_witnesses.len() <= MAX_PROOF_OBJECTS
            && material.bundle.journal.len() <= MAX_PROOF_OBJECTS,
        "private recovery checkpoint context mismatch",
    )?;
    let account_pin = material
        .pins
        .iter()
        .find(|pin| pin.domain == PolicyDomain::Account(material.scope.account_id.clone()))
        .ok_or(AuthorizationError::InvalidProof(
            "missing checkpoint Account pin",
        ))?;
    let space_pin = material
        .pins
        .iter()
        .find(|pin| pin.domain == PolicyDomain::Space(material.scope.space_id.clone()))
        .ok_or(AuthorizationError::InvalidProof(
            "missing checkpoint PersonalSpace pin",
        ))?;
    require(
        account_pin.genesis_root == space_pin.genesis_root,
        "checkpoint changed personal genesis",
    )?;
    let mut state = VerifiedPolicySet::new();
    state.trust_account_genesis(&account_pin.genesis_root, value.account_genesis.clone())?;
    state.trust_space_genesis(&space_pin.genesis_root, value.space_genesis.clone())?;
    budget.verify(&value.account_genesis)?;
    budget.verify(&value.space_genesis)?;
    let mut authority = value.account_genesis.payload.clone();
    let mut roots = BTreeMap::from([(authority.root_generation, authority.current_root.clone())]);
    let mut changes = vec![value.account_genesis.clone()];
    let mut previous_stamp = account_stamp(&value.account_genesis)?;
    for signed in &value.authority_changes {
        let next = &signed.payload;
        let stamp = account_stamp(signed)?;
        require(
            next.account == authority.account
                && next.genesis_root == authority.genesis_root
                && (next.current_root != authority.current_root
                    || next.recovery != authority.recovery
                    || next.authority_epoch != authority.authority_epoch
                    || next.authority != authority.authority)
                && next.authority_epoch >= authority.authority_epoch
                && (next.authority == authority.authority
                    || next.authority_epoch > authority.authority_epoch)
                && (stamp.authority_epoch, stamp.sequence)
                    > (previous_stamp.authority_epoch, previous_stamp.sequence)
                && next.previous.is_some(),
            "invalid checkpoint authority lineage",
        )?;
        state.validate_account(next)?;
        let bytes = next.governance_bytes()?;
        validate_root_transition(
            &signed.issuer,
            &authority.current_root,
            &next.current_root,
            authority.root_generation,
            next.root_generation,
            &next.root_acceptance,
            "account-root-accept-v2",
            &bytes,
            budget,
        )?;
        let recovery = signed.issuer != authority.current_root;
        if recovery || next.recovery != authority.recovery {
            recovery_approved(
                &authority.recovery,
                &next.recovery_approvals,
                "account-recovery-v2",
                &bytes,
                budget,
            )?;
        } else {
            require(
                next.recovery_approvals.is_empty(),
                "unexpected checkpoint recovery approvals",
            )?;
        }
        if recovery {
            require(
                authority.recovery.keys.contains(&signed.issuer)
                    && next.current_root != authority.current_root
                    && next.authority_epoch > authority.authority_epoch,
                "unauthorized checkpoint recovery transition",
            )?;
        }
        validate_recovery_transition(&authority.recovery, &next.recovery)?;
        budget.verify(signed)?;
        roots.insert(next.root_generation, next.current_root.clone());
        authority = next.clone();
        previous_stamp = stamp;
        changes.push(signed.clone());
    }
    require(
        value.account.payload.account == material.scope.account_id
            && value.account.payload.account == authority.account
            && value.account.payload.genesis_root == account_pin.genesis_root
            && value.account.payload.current_root == authority.current_root
            && value.account.payload.root_generation == authority.root_generation
            && value.account.payload.authority_epoch == authority.authority_epoch
            && value.account.payload.authority == authority.authority
            && value.account.payload.recovery == authority.recovery
            && checkpoint.snapshot.issuer == authority.current_root
            && authority.recovery.threshold == 1
            && authority.recovery.keys == [checkpoint.recovery_approval.key.clone()]
            && value.space.payload.space == material.scope.space_id
            && value.space.payload.root
                == (SpaceRoot::Personal {
                    owner: material.scope.account_id.clone(),
                    genesis_root: account_pin.genesis_root.clone(),
                })
            && value.space.payload.current_root == authority.current_root
            && value.space.payload.root_generation == authority.root_generation
            && value.space.payload.recovery == authority.recovery
            && account_pin.minimum_head.as_ref() == Some(&account_stamp(&value.account)?)
            && space_pin.minimum_head.as_ref() == Some(&space_stamp(&value.space)?),
        "checkpoint changed Root authority, personal scope, or exact saved heads",
    )?;
    budget.verify(&checkpoint.snapshot)?;
    verify_approval(
        &checkpoint.recovery_approval.key,
        APPROVAL_DOMAIN,
        &canonical(&checkpoint.snapshot)?,
        &checkpoint.recovery_approval.signature,
        budget,
    )?;
    let mut account_history = BTreeMap::new();
    let mut space_history = BTreeMap::new();
    for signed in changes {
        account_history.insert(account_stamp(&signed)?, signed);
    }
    space_history.insert(
        space_stamp(&value.space_genesis)?,
        value.space_genesis.clone(),
    );
    for witness in &value.policy_witnesses {
        match witness {
            PolicyWitness::Account(signed) => {
                account_history.insert(account_stamp(signed)?, signed.clone());
            }
            PolicyWitness::Space(signed) => {
                space_history.insert(space_stamp(signed)?, signed.clone());
            }
        }
    }
    account_history.insert(account_stamp(&value.account)?, value.account.clone());
    space_history.insert(space_stamp(&value.space)?, value.space.clone());
    for (stamp, signed) in &account_history {
        require(
            signed.payload.account == material.scope.account_id
                && signed.payload.genesis_root == account_pin.genesis_root
                && roots.get(&signed.payload.root_generation) == Some(&signed.payload.current_root)
                && (stamp.authority_epoch, stamp.sequence)
                    <= (
                        value.account.payload.authority_epoch,
                        value.account.payload.sequence,
                    ),
            "checkpoint Account witness has an unknown authority",
        )?;
        budget.verify(signed)?;
        state
            .roots
            .insert(stamp.clone(), signed.payload.current_root.clone());
    }
    for (stamp, signed) in &space_history {
        require(
            signed.payload.space == material.scope.space_id
                && signed.payload.root == value.space.payload.root
                && roots.get(&signed.payload.root_generation) == Some(&signed.payload.current_root)
                && (stamp.authority_epoch, stamp.sequence)
                    <= (
                        value.space.payload.authority_epoch,
                        value.space.payload.sequence,
                    ),
            "checkpoint Space witness has an unknown authority",
        )?;
        budget.verify(signed)?;
        state
            .roots
            .insert(stamp.clone(), signed.payload.current_root.clone());
    }
    state.accounts.insert(
        material.scope.account_id.clone(),
        AccountState {
            history: account_history.into_values().collect(),
            stamp: account_stamp(&value.account)?,
        },
    );
    state.spaces.insert(
        material.scope.space_id.clone(),
        SpaceState {
            history: space_history.into_values().collect(),
            stamp: space_stamp(&value.space)?,
        },
    );
    state.validate_account(&value.account.payload)?;
    state.validate_space(&value.space.payload)?;
    let mut seen = BTreeSet::new();
    for entry in &material.bundle.journal {
        require(
            seen.insert(object_hash(entry)?),
            "duplicate private checkpoint object",
        )?;
        let stamp = object_stamp(entry)?;
        require(
            state.roots.contains_key(&stamp),
            "checkpoint object lacks original policy witness",
        )?;
        match entry {
            JournalEntry::Device(signed) => {
                budget.verify(signed)?;
                state.devices.insert(signed.hash()?, signed.clone());
            }
            JournalEntry::Resource(signed) => {
                budget.verify(signed)?;
                state.install_resource(signed.clone())?;
            }
            JournalEntry::Grant(signed) => {
                budget.verify(signed)?;
                state.grants.insert(signed.hash()?, signed.clone());
            }
            JournalEntry::Authenticator(signed) => {
                budget.verify(signed)?;
                state.authenticators.insert(signed.hash()?, signed.clone());
            }
            _ => {
                return Err(AuthorizationError::InvalidProof(
                    "invalid private checkpoint object carrier",
                ));
            }
        }
    }
    state.validate_device_registration(
        &PolicyDomain::Account(material.scope.account_id.clone()),
        &value.account.payload.devices,
        &value.account.payload.revoked_devices,
        &value.account.payload.current_root,
    )?;
    state.validate_space(&value.space.payload)?;
    state.validate_authenticator_registration(&value.account.payload)?;
    for (hash, floor) in &value.authenticator_states {
        let record = state
            .authenticators
            .get(hash)
            .ok_or(AuthorizationError::InvalidProof(
                "missing checkpoint authenticator",
            ))?;
        state.validate_authenticator(record, record.payload.not_before_ms)?;
        require(
            value.account.payload.authenticators.contains(hash)
                && floor.sign_count >= record.payload.initial_state.sign_count
                && (!floor.backup_state || record.payload.backup_eligible)
                && state
                    .authenticator_states
                    .insert(hash.clone(), *floor)
                    .is_none(),
            "invalid checkpoint authenticator floor",
        )?;
    }
    require(
        state.authenticator_states.len() == value.account.payload.authenticators.len(),
        "checkpoint omitted an authenticator floor",
    )?;
    let at = state
        .find_device(&material.scope.device_id)?
        .payload
        .not_before_ms;
    scope_budget(&state, material, at, budget)?;
    Ok(state)
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

    fn claim(now: f64, nonce_byte: u8) -> VerifiedNodeClaim {
        let nonce = Nonce::from_bytes([nonce_byte; 32]).to_string();
        verify_node_claim(
            &create_node_claim(&key(4), &nonce, now, now + 100_000.0).unwrap(),
            &key(4).public_key_hex(),
            &nonce,
            now,
        )
        .unwrap()
    }

    fn inspect_with_budget(material: &str, now: f64) -> serde_json::Value {
        let signatures = crate::signature_verification::SignatureVerification::new();
        signatures
            .run(|| inspect_account_recovery_material(material, now))
            .unwrap();
        let material = read_material(material).unwrap();
        let mut budget = Budget::default();
        material_state_with_budget(&material, &mut budget).unwrap();
        assert!(budget.objects.len() <= MAX_PROOF_OBJECTS);
        assert!(budget.signatures.len() <= MAX_PROOF_SIGNATURES);
        assert!(budget.bytes <= MAX_PROOF_BYTES);
        assert!(signatures.counts().actual_verifications <= MAX_PROOF_SIGNATURES);
        serde_json::json!({
            "material_bytes": canonical(&material).unwrap().len(),
            "proof_objects": budget.objects.len(),
            "proof_bytes": budget.bytes,
            "proof_signatures": budget.signatures.len(),
            "crypto": signatures.counts(),
        })
    }

    #[test]
    fn generated_checkpoint_meter_counts_nested_proof_without_crypto() {
        let created = super::super::tests::account_with_enrolled_nodes(12);
        let encoded = build_account_recovery_material(
            &created,
            &key(1),
            &key(2),
            "https://node.example",
            10_000.0,
        )
        .unwrap();
        let material = read_material(&encoded).unwrap();
        assert!(material.policy_checkpoint.is_some());
        let signatures = crate::signature_verification::SignatureVerification::new();
        let usage = signatures
            .run(|| recovery_material_usage(&material))
            .unwrap();
        assert_eq!(signatures.counts().actual_verifications, 0);
        assert!(usage.counts().unique_signed_objects > material.bundle.journal.len());
        usage.enforce_limits().unwrap();
    }

    fn recover(
        material: &str,
        current: &CreatedAccount,
        root: u8,
        client: u8,
        now: f64,
    ) -> CreatedAccount {
        let prepared = prepare_account_recovery_from_bundle(
            material,
            &current.bundle_json().unwrap(),
            &key(root),
            &key(client),
            &claim(now, root + 100),
            END,
            now,
        )
        .unwrap();
        let possession = sign_recovery_node_possession(
            &prepared.node_request_json().unwrap(),
            &key(4),
            &current.pins_json().unwrap(),
            &current.bundle_json().unwrap(),
            &Nonce::from_bytes([root + 100; 32]).to_string(),
            now,
        )
        .unwrap();
        finish_account_recovery(&prepared, &key(root), &possession, now).unwrap()
    }

    fn approve_again(material: &mut RecoveryMaterial, root: u8) {
        let checkpoint = material.policy_checkpoint.as_mut().unwrap();
        checkpoint.snapshot =
            Signed::sign(checkpoint.snapshot.payload.clone(), &key(root)).unwrap();
        checkpoint.recovery_approval = KeyApproval {
            key: PublicKey::of(&key(2)),
            signature: Signature::parse(
                &key(2)
                    .sign(APPROVAL_DOMAIN, &canonical(&checkpoint.snapshot).unwrap())
                    .unwrap(),
            )
            .unwrap(),
        };
    }

    #[test]
    fn ordinary_checkpoint_survives_twelve_node_enrollments_and_repeated_recovery() {
        let original = super::super::tests::account_with_enrolled_nodes(12);
        let node_commits = |created: &CreatedAccount| {
            created
                .bundle
                .journal
                .iter()
                .filter(|entry| matches!(entry, JournalEntry::AccountCommit(_)))
                .count()
        };
        assert_eq!(node_commits(&original), 12);
        assert!(original.bundle_json().unwrap().len() > MAX_RECOVERY_MATERIAL_BYTES);
        let initial =
            build_account_recovery_material(&original, &key(1), &key(2), ENDPOINT, NOW).unwrap();
        let compact = read_material(&initial).unwrap();
        assert!(compact.policy_checkpoint.is_some());
        assert!(initial.len() <= MAX_RECOVERY_MATERIAL_BYTES);
        assert!(replay_policies(&compact.pins, &compact.bundle.journal).is_err());
        let mut evidence = vec![inspect_with_budget(&initial, NOW)];
        context::build_recovery_context_request(
            &initial,
            &key(50),
            &key(4).public_key_hex(),
            RESTORE,
            RESTORE + 1_000.0,
        )
        .unwrap();
        cross_node::inspect_cross_node_recovery_sources(&initial, RESTORE).unwrap();
        let sealed = crate::recovery::recovery_seal(
            initial.as_bytes(),
            "synthetic checkpoint backup password",
        )
        .unwrap();
        let opened =
            crate::recovery::recovery_open(&sealed, "synthetic checkpoint backup password")
                .unwrap();
        assert_eq!(opened, initial.as_bytes());
        let opened = std::str::from_utf8(&opened).unwrap();
        inspect_account_recovery_material(opened, RESTORE).unwrap();
        let first = recover(opened, &original, 5, 6, RESTORE);
        assert_eq!(node_commits(&first), 12);
        let first_material =
            build_recovered_account_recovery_material(&first, &key(5), &initial, ENDPOINT, RESTORE)
                .unwrap();
        evidence.push(inspect_with_budget(&first_material, RESTORE));
        let second = recover(&first_material, &first, 7, 8, RESTORE + 1_000.0);
        assert_eq!(node_commits(&second), 12);
        let second_material = build_recovered_account_recovery_material(
            &second,
            &key(7),
            &first_material,
            ENDPOINT,
            RESTORE + 1_000.0,
        )
        .unwrap();
        evidence.push(inspect_with_budget(&second_material, RESTORE + 1_000.0));
        let third = recover(&second_material, &second, 9, 10, RESTORE + 2_000.0);
        assert_eq!(node_commits(&third), 12);
        assert_eq!(
            third.authorization.scope.account_id,
            original.authorization.scope.account_id
        );

        let mut changed = read_material(&first_material).unwrap();
        changed.bundle.grant_leaf = ObjectHash::body(&"changed checkpoint object").unwrap();
        assert!(validate_material(&changed).is_err());
        let mut changed = read_material(&first_material).unwrap();
        changed.pins[0].minimum_head = Some(
            account_stamp(
                &changed
                    .policy_checkpoint
                    .as_ref()
                    .unwrap()
                    .snapshot
                    .payload
                    .account_genesis,
            )
            .unwrap(),
        );
        changed
            .policy_checkpoint
            .as_mut()
            .unwrap()
            .snapshot
            .payload
            .pins = changed.pins.clone();
        approve_again(&mut changed, 5);
        assert!(validate_material(&changed).is_err());
        let mut changed = read_material(&first_material).unwrap();
        let snapshot = &mut changed.policy_checkpoint.as_mut().unwrap().snapshot.payload;
        let mut account = snapshot.account.payload.clone();
        account.authority_epoch += 1;
        snapshot.account = Signed::sign(account, &key(5)).unwrap();
        let head = account_stamp(&snapshot.account).unwrap();
        changed
            .pins
            .iter_mut()
            .find(|pin| pin.domain == head.domain)
            .unwrap()
            .minimum_head = Some(head.clone());
        snapshot.pins = changed.pins.clone();
        approve_again(&mut changed, 5);
        assert!(validate_material(&changed).is_err());
        let mut changed = read_material(&first_material).unwrap();
        let snapshot = &mut changed.policy_checkpoint.as_mut().unwrap().snapshot.payload;
        snapshot.account.payload.current_root = PublicKey::of(&key(99));
        snapshot.space.payload.current_root = PublicKey::of(&key(99));
        approve_again(&mut changed, 99);
        assert!(validate_material(&changed).is_err());
        let mut changed = read_material(&first_material).unwrap();
        changed
            .policy_checkpoint
            .as_mut()
            .unwrap()
            .recovery_approval
            .key = PublicKey::of(&key(99));
        assert!(validate_material(&changed).is_err());
        let mut changed = read_material(&first_material).unwrap();
        changed.version = 1;
        assert!(validate_material(&changed).is_err());
        let mut changed = read_material(&first_material).unwrap();
        changed
            .policy_checkpoint
            .as_mut()
            .unwrap()
            .snapshot
            .payload
            .version = 1;
        approve_again(&mut changed, 5);
        assert!(validate_material(&changed).is_err());
        let mut changed = read_material(&first_material).unwrap();
        changed
            .policy_checkpoint
            .as_mut()
            .unwrap()
            .snapshot
            .payload
            .authority_changes
            .clear();
        approve_again(&mut changed, 5);
        assert!(validate_material(&changed).is_err());
        println!(
            "checkpoint lifecycle evidence: {}",
            serde_json::json!({
                "root_approved_node_enrollments": node_commits(&original),
                "full_public_bundle_bytes": original.bundle_json().unwrap().len(),
                "same_node_recoveries": 3,
                "private_backups": evidence,
            })
        );
    }
}
