use super::{execution_handoff::ExecutionProofUsage, *};

struct PolicyProof<'a> {
    state: &'a VerifiedPolicySet,
    usage: ExecutionProofUsage,
    raw: Budget,
    devices: BTreeSet<ObjectHash>,
    grants: BTreeSet<ObjectHash>,
    now_ms: u64,
}

impl PolicyProof<'_> {
    fn signed<T: SignedObject>(&mut self, value: &Signed<T>) -> Result<()> {
        self.usage.record_signed(value)?;
        self.usage.enforce_limits()
    }

    fn signature(
        &mut self,
        key: &PublicKey,
        domain: &str,
        body: &[u8],
        signature: &Signature,
    ) -> Result<()> {
        self.raw.signatures.insert((
            key.clone(),
            domain.to_owned(),
            ObjectHash(format!("h2_{}", digest(domain, body))),
            signature.clone(),
        ));
        self.usage.record_budget(&self.raw)?;
        self.usage.enforce_limits()
    }

    fn assertion(&mut self, proof: &crate::webauthn::PasskeyAssertionProof) -> Result<()> {
        self.signed(&proof.record)?;
        self.usage.record_metadata(proof)?;
        self.raw
            .external_signatures
            .insert(ObjectHash::body(proof)?);
        self.usage.record_budget(&self.raw)?;
        self.usage.enforce_limits()
    }

    fn account_object(&mut self, value: &Signed<AccountDocument>) -> Result<()> {
        self.signed(value)?;
        let document = &value.payload;
        if document.root_acceptance.is_some() || !document.recovery_approvals.is_empty() {
            let body = document.governance_bytes()?;
            if let Some(signature) = &document.root_acceptance {
                self.signature(
                    &document.current_root,
                    "account-root-accept-v2",
                    &body,
                    signature,
                )?;
            }
            for approval in &document.recovery_approvals {
                self.signature(
                    &approval.key,
                    "account-recovery-v2",
                    &body,
                    &approval.signature,
                )?;
            }
        }
        if let Some(approval) = &document.authenticator_approval {
            self.signed(&approval.intent)?;
            self.assertion(&approval.assertion)?;
        }
        if let Some(proof) = &document.device_enrollment {
            self.signed(&proof.intent)?;
            self.device_object(&proof.certificate)?;
            self.assertion(&proof.assertion)?;
        }
        Ok(())
    }

    fn space_object(&mut self, value: &Signed<SpaceDocument>) -> Result<()> {
        self.signed(value)?;
        let document = &value.payload;
        if document.root_acceptance.is_some() || !document.recovery_approvals.is_empty() {
            let body = document.governance_bytes()?;
            if let Some(signature) = &document.root_acceptance {
                self.signature(
                    &document.current_root,
                    "space-root-accept-v2",
                    &body,
                    signature,
                )?;
            }
            for approval in &document.recovery_approvals {
                self.signature(
                    &approval.key,
                    "space-recovery-v2",
                    &body,
                    &approval.signature,
                )?;
            }
        }
        Ok(())
    }

    fn device_object(&mut self, value: &Signed<DeviceCertificate>) -> Result<()> {
        self.signed(value)?;
        if let Some(signature) = &value.payload.possession {
            self.signature(
                &value.payload.public_key,
                "device-possession-v2",
                &value.payload.possession_bytes()?,
                signature,
            )?;
        }
        Ok(())
    }

    fn witness(&mut self, stamp: &PolicyStamp) -> Result<()> {
        require(
            self.state.roots.contains_key(stamp),
            "missing exact policy proof witness",
        )?;
        match &stamp.domain {
            PolicyDomain::Account(account) => {
                let value = self
                    .state
                    .account_history(account)
                    .and_then(|history| {
                        history.iter().find(|value| {
                            value.payload.authority_epoch == stamp.authority_epoch
                                && value.payload.sequence == stamp.sequence
                        })
                    })
                    .ok_or(AuthorizationError::PolicyConflict)?;
                self.signed(value)
            }
            PolicyDomain::Space(space) => {
                let value = self
                    .state
                    .space_history(space)
                    .and_then(|history| {
                        history.iter().find(|value| {
                            value.payload.authority_epoch == stamp.authority_epoch
                                && value.payload.sequence == stamp.sequence
                        })
                    })
                    .ok_or(AuthorizationError::PolicyConflict)?;
                self.signed(value)
            }
        }
    }

    fn current(&mut self, domain: &PolicyDomain) -> Result<()> {
        let stamp = self
            .state
            .head(domain)
            .ok_or(AuthorizationError::InvalidProof(
                "missing current policy proof",
            ))?;
        self.witness(stamp)?;
        if let PolicyDomain::Space(space) = domain {
            if let Some(SpaceDocument {
                root: SpaceRoot::Personal { owner, .. },
                ..
            }) = self.state.space(space)
            {
                let owner = owner.clone();
                self.current(&PolicyDomain::Account(owner))?;
            }
        }
        Ok(())
    }

    fn signer(&mut self, key: &PublicKey, depth: usize) -> Result<Actor> {
        let certificate = self.state.find_device(&DeviceId::from_key(key))?;
        self.device(certificate, depth)?;
        Ok(certificate.payload.subject.clone())
    }

    fn device(&mut self, value: &Signed<DeviceCertificate>, depth: usize) -> Result<()> {
        require(
            depth <= MAX_DELEGATION_DEPTH,
            "policy device proof exceeds delegation depth",
        )?;
        self.usage.record_chain_depth(depth);
        let hash = value.hash()?;
        if !self.devices.insert(hash.clone()) {
            return Ok(());
        }
        self.signed(value)?;
        self.witness(&value.payload.issued_under)?;
        self.current(&value.payload.owner)?;
        let root = self.state.historical_root(&value.payload.issued_under)?;
        if value.issuer != root {
            match &value.payload.owner {
                PolicyDomain::Space(_) => {
                    let grant = value.payload.enrollment_grant.as_ref().ok_or(
                        AuthorizationError::InvalidProof("missing service enrollment grant proof"),
                    )?;
                    self.grant(grant, 1)?;
                    self.signer(&value.issuer, depth + 1)?;
                }
                PolicyDomain::Account(_) => {
                    let (registered, revoked) =
                        self.state.active_device_hashes(&value.payload.owner)?;
                    if !registered.contains(&hash) || revoked.contains(&value.payload.device) {
                        self.signer(&value.issuer, depth + 1)?;
                    }
                }
            }
        }
        Ok(())
    }

    fn selector(&mut self, selector: &MembersOf, actor: &Actor) -> Result<()> {
        self.current(&PolicyDomain::Space(selector.space.clone()))?;
        let genesis = self
            .state
            .space_history(&selector.space)
            .and_then(|history| history.first())
            .ok_or(AuthorizationError::PolicyConflict)?;
        self.signed(genesis)?;
        let space = self
            .state
            .space(&selector.space)
            .ok_or(AuthorizationError::PolicyConflict)?;
        for hash in &space.memberships {
            let membership =
                self.state
                    .memberships
                    .get(hash)
                    .ok_or(AuthorizationError::InvalidProof(
                        "missing selector membership proof",
                    ))?;
            if &membership.payload.subject != actor
                || !membership
                    .payload
                    .roles
                    .iter()
                    .any(|role| selector.roles_any.contains(role))
                || membership.payload.not_before_ms > self.now_ms
                || self.now_ms >= membership.payload.expires_at_ms
            {
                continue;
            }
            self.signed(membership)?;
            self.witness(&membership.payload.issued_under)?;
            if membership.issuer
                != self
                    .state
                    .historical_root(&membership.payload.issued_under)?
            {
                self.signer(&membership.issuer, 1)?;
            }
            break;
        }
        Ok(())
    }

    fn grant(&mut self, hash: &ObjectHash, depth: usize) -> Result<()> {
        require(
            depth <= MAX_DELEGATION_DEPTH,
            "policy grant proof exceeds delegation depth",
        )?;
        self.usage.record_chain_depth(depth);
        if !self.grants.insert(hash.clone()) {
            return Ok(());
        }
        let value = self
            .state
            .grants
            .get(hash)
            .ok_or(AuthorizationError::InvalidProof(
                "missing policy grant proof",
            ))?;
        self.grant_object(value, depth)
    }

    fn grant_object(&mut self, value: &Signed<Grant>, depth: usize) -> Result<()> {
        require(
            depth <= MAX_DELEGATION_DEPTH,
            "policy grant proof exceeds delegation depth",
        )?;
        self.usage.record_chain_depth(depth);
        self.signed(value)?;
        self.witness(&value.payload.issued_under)?;
        self.current(&PolicyDomain::Space(value.payload.owner_space.clone()))?;
        if let Grantee::MembersOf(selector) = &value.payload.grantee {
            let genesis = self
                .state
                .space_history(&selector.space)
                .and_then(|history| history.first())
                .ok_or(AuthorizationError::PolicyConflict)?;
            self.signed(genesis)?;
        }
        if let Some(parent) = &value.payload.parent {
            self.grant(parent, depth + 1)?;
            let actor = self.signer(&value.issuer, 1)?;
            let parent = self
                .state
                .grants
                .get(parent)
                .ok_or(AuthorizationError::InvalidProof(
                    "missing parent grant proof",
                ))?;
            if let Grantee::MembersOf(selector) = &parent.payload.grantee {
                self.selector(selector, &actor)?;
            }
        }
        Ok(())
    }

    fn account_refs(
        &mut self,
        document: &AccountDocument,
        embedded: Option<&Signed<DeviceCertificate>>,
    ) -> Result<()> {
        for hash in &document.devices {
            if let Some(value) = self.state.devices.get(hash) {
                self.signed(value)?;
            } else {
                let value = embedded.ok_or(AuthorizationError::InvalidProof(
                    "missing policy device proof",
                ))?;
                require(
                    value.hash()? == *hash,
                    "embedded policy device proof mismatch",
                )?;
                self.device_object(value)?;
            }
        }
        for hash in &document.authenticators {
            self.signed(self.state.authenticators.get(hash).ok_or(
                AuthorizationError::InvalidProof("missing policy authenticator proof"),
            )?)?;
        }
        Ok(())
    }

    fn account_transition(&mut self, value: &Signed<AccountDocument>) -> Result<()> {
        self.account_object(value)?;
        let document = &value.payload;
        if let Some(previous) = self
            .state
            .account_history(&document.account)
            .and_then(|history| history.last())
        {
            self.signed(previous)?;
            self.account_refs(&previous.payload, None)?;
            if value.issuer != previous.payload.current_root
                && !previous.payload.recovery.keys.contains(&value.issuer)
                && !(document.device_enrollment.is_some()
                    && document.device_enrollment != previous.payload.device_enrollment)
            {
                self.signer(&value.issuer, 1)?;
            }
        }
        self.account_refs(
            document,
            document
                .device_enrollment
                .as_ref()
                .map(|proof| &proof.certificate),
        )?;
        if let Some(proof) = &document.device_enrollment {
            self.witness(&proof.certificate.payload.issued_under)?;
            for head in &proof.intent.payload.expected_heads {
                self.witness(head)?;
            }
        }
        if let Some(approval) = &document.authenticator_approval {
            for head in &approval.intent.payload.expected_heads {
                self.witness(head)?;
            }
        }
        Ok(())
    }

    fn space_refs(&mut self, document: &SpaceDocument) -> Result<()> {
        for hash in &document.devices {
            self.signed(
                self.state
                    .devices
                    .get(hash)
                    .ok_or(AuthorizationError::InvalidProof(
                        "missing space device proof",
                    ))?,
            )?;
        }
        for hash in &document.resources {
            self.signed(self.state.resources.get(hash).ok_or(
                AuthorizationError::InvalidProof("missing space resource proof"),
            )?)?;
        }
        for hash in &document.grants {
            self.signed(
                self.state
                    .grants
                    .get(hash)
                    .ok_or(AuthorizationError::InvalidProof(
                        "missing space grant proof",
                    ))?,
            )?;
        }
        for hash in &document.memberships {
            self.signed(self.state.memberships.get(hash).ok_or(
                AuthorizationError::InvalidProof("missing space membership proof"),
            )?)?;
        }
        Ok(())
    }

    fn space_transition(&mut self, value: &Signed<SpaceDocument>) -> Result<()> {
        self.space_object(value)?;
        let document = &value.payload;
        let mut governing_root = document.current_root.clone();
        if let SpaceRoot::Personal { owner, .. } = &document.root {
            self.current(&PolicyDomain::Account(owner.clone()))?;
            governing_root = self
                .state
                .account(owner)
                .ok_or(AuthorizationError::PolicyConflict)?
                .current_root
                .clone();
        }
        if let Some(previous) = self
            .state
            .space_history(&document.space)
            .and_then(|history| history.last())
        {
            self.signed(previous)?;
            self.space_refs(&previous.payload)?;
            if matches!(document.root, SpaceRoot::Team { .. }) {
                governing_root = previous.payload.current_root.clone();
            }
            if value.issuer != governing_root
                && !previous.payload.recovery.keys.contains(&value.issuer)
            {
                let actor = self.signer(&value.issuer, 1)?;
                let hash =
                    document
                        .governance_grant
                        .as_ref()
                        .ok_or(AuthorizationError::InvalidProof(
                            "missing space governance grant proof",
                        ))?;
                self.grant(hash, 1)?;
                let grant = self
                    .state
                    .grants
                    .get(hash)
                    .ok_or(AuthorizationError::PolicyConflict)?;
                if let Grantee::MembersOf(selector) = &grant.payload.grantee {
                    self.selector(selector, &actor)?;
                }
                for hash in &document.grants {
                    if !previous.payload.grants.contains(hash) {
                        self.grant(hash, 1)?;
                    }
                }
            }
        }
        self.space_refs(document)
    }

    fn committed(
        &mut self,
        commit: &Signed<PolicyCommit>,
        receipts: &[Signed<FreshnessReceipt>],
    ) -> Result<()> {
        self.signed(commit)?;
        self.witness(&commit.payload.previous)?;
        for receipt in receipts {
            self.signed(receipt)?;
        }
        for dependency in &commit.payload.dependencies {
            self.witness(dependency)?;
            let authority = match &dependency.domain {
                PolicyDomain::Account(account) => {
                    self.state.account(account).map(|value| &value.authority)
                }
                PolicyDomain::Space(space) => self.state.space(space).map(|value| &value.authority),
            }
            .ok_or(AuthorizationError::PolicyConflict)?;
            if self
                .state
                .devices
                .values()
                .any(|value| value.payload.device == authority.device)
            {
                self.device(self.state.find_device(&authority.device)?, 1)?;
            }
        }
        Ok(())
    }
}

/// Counts the entry's carried proof without verifying it or loading historical dependencies.
pub fn policy_entry_payload_usage(entry: &JournalEntry) -> Result<ExecutionProofUsage> {
    let state = VerifiedPolicySet::new();
    let mut meter = PolicyProof {
        state: &state,
        usage: ExecutionProofUsage::default(),
        raw: Budget::default(),
        devices: BTreeSet::new(),
        grants: BTreeSet::new(),
        now_ms: 0,
    };
    match entry {
        JournalEntry::AccountGenesis(value) | JournalEntry::Account(value) => {
            meter.account_object(value)?
        }
        JournalEntry::SpaceGenesis(value) | JournalEntry::Space(value) => {
            meter.space_object(value)?
        }
        JournalEntry::AccountCommit(value) => {
            meter.account_object(&value.proposal)?;
            meter.signed(&value.commit)?;
            for receipt in &value.receipts {
                meter.signed(receipt)?;
            }
        }
        JournalEntry::SpaceCommit(value) => {
            meter.space_object(&value.proposal)?;
            meter.signed(&value.commit)?;
            for receipt in &value.receipts {
                meter.signed(receipt)?;
            }
        }
        JournalEntry::Device(value) => meter.device_object(value)?,
        JournalEntry::Resource(value) => meter.signed(value)?,
        JournalEntry::Authenticator(value) => meter.signed(value)?,
        JournalEntry::Membership(value) => meter.signed(value)?,
        JournalEntry::Grant(value) => meter.signed(value)?,
    }
    meter.usage.record_budget(&meter.raw)?;
    meter.usage.enforce_limits()?;
    Ok(meter.usage)
}

/// Counts verification inputs against the exact causal state preceding this entry.
pub fn policy_entry_usage(
    state: &VerifiedPolicySet,
    entry: &JournalEntry,
) -> Result<ExecutionProofUsage> {
    let now_ms = match entry {
        JournalEntry::AccountCommit(value) => value.commit.payload.accepted_at_ms,
        JournalEntry::SpaceCommit(value) => value.commit.payload.accepted_at_ms,
        JournalEntry::Device(value) => value.payload.not_before_ms,
        JournalEntry::Authenticator(value) => value.payload.not_before_ms,
        JournalEntry::Membership(value) => value.payload.not_before_ms,
        JournalEntry::Grant(value) => value.payload.not_before_ms,
        _ => 0,
    };
    let mut meter = PolicyProof {
        state,
        usage: ExecutionProofUsage::default(),
        raw: Budget::default(),
        devices: BTreeSet::new(),
        grants: BTreeSet::new(),
        now_ms,
    };
    match entry {
        JournalEntry::AccountGenesis(value) | JournalEntry::Account(value) => {
            meter.account_transition(value)?
        }
        JournalEntry::AccountCommit(value) => {
            meter.account_transition(&value.proposal)?;
            meter.committed(&value.commit, &value.receipts)?;
        }
        JournalEntry::SpaceGenesis(value) | JournalEntry::Space(value) => {
            meter.space_transition(value)?
        }
        JournalEntry::SpaceCommit(value) => {
            meter.space_transition(&value.proposal)?;
            meter.committed(&value.commit, &value.receipts)?;
        }
        JournalEntry::Device(value) => {
            meter.device_object(value)?;
            meter.device(value, 1)?;
        }
        JournalEntry::Resource(value) => {
            meter.signed(value)?;
            meter.witness(&value.payload.issued_under)?;
        }
        JournalEntry::Authenticator(value) => {
            meter.signed(value)?;
            meter.witness(&value.payload.issued_under)?;
            meter.current(&PolicyDomain::Account(value.payload.account.clone()))?;
            if value.issuer != state.historical_root(&value.payload.issued_under)? {
                meter.signer(&value.issuer, 1)?;
            }
        }
        JournalEntry::Membership(value) => {
            meter.signed(value)?;
            meter.witness(&value.payload.issued_under)?;
            if value.issuer != state.historical_root(&value.payload.issued_under)? {
                meter.signer(&value.issuer, 1)?;
            }
        }
        JournalEntry::Grant(value) => meter.grant_object(value, 1)?,
    }
    meter.usage.record_budget(&meter.raw)?;
    meter.usage.enforce_limits()?;
    Ok(meter.usage)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{authorization::tests::Fixture, signature_verification::SignatureVerification};

    fn next(fixture: &Fixture) -> AccountDocument {
        let mut document = fixture.state.account(&fixture.account).unwrap().clone();
        document.previous = fixture
            .state
            .head(&PolicyDomain::Account(fixture.account.clone()))
            .cloned();
        document.sequence += 1;
        document
    }

    #[test]
    fn meters_current_account_dependencies_without_crypto_or_prefix_replay() {
        let fixture = Fixture::new();
        let entry = JournalEntry::Account(Signed::sign(next(&fixture), &fixture.root).unwrap());
        let scope = SignatureVerification::new();
        let usage = scope
            .run(|| policy_entry_usage(&fixture.state, &entry))
            .unwrap();
        assert_eq!(scope.counts().actual_verifications, 0);
        assert_eq!(scope.counts().cache_hits, 0);
        let counts = usage.counts();
        assert_eq!(counts.unique_signed_objects, 4);
        assert_eq!(counts.unique_signatures, 4);
        assert!(counts.authorization_bytes > canonical(&entry).unwrap().len());
        assert!(counts.authorization_bytes <= MAX_PROOF_BYTES);
    }

    #[test]
    fn meters_root_acceptance_as_an_additional_signature() {
        let fixture = Fixture::new();
        let root = Identity::from_secret_hex(&hex::encode([21; 32])).unwrap();
        let mut document = next(&fixture);
        document.current_root = PublicKey::of(&root);
        document.root_generation += 1;
        document.accept_root(&root).unwrap();
        let entry = JournalEntry::Account(Signed::sign(document, &fixture.root).unwrap());
        let usage = policy_entry_usage(&fixture.state, &entry).unwrap();
        assert_eq!(usage.counts().unique_signed_objects, 4);
        assert_eq!(usage.counts().unique_signatures, 5);
    }

    #[test]
    fn a_new_device_entry_includes_its_possession_signature() {
        let fixture = Fixture::new();
        let identity = Identity::from_secret_hex(&hex::encode([61; 32])).unwrap();
        let key = PublicKey::of(&identity);
        let mut certificate = fixture
            .state
            .devices
            .values()
            .next()
            .unwrap()
            .payload
            .clone();
        certificate.subject = Actor::Account(fixture.account.clone());
        certificate.device = DeviceId::from_key(&key);
        certificate.public_key = key;
        certificate.uses = vec![DeviceUse::Noise, DeviceUse::AccountInvocation];
        certificate.nonce = Nonce::from_bytes([61; 32]);
        certificate.prove_possession(&identity).unwrap();
        let entry = JournalEntry::Device(Signed::sign(certificate, &fixture.root).unwrap());
        let usage = policy_entry_usage(&fixture.state, &entry).unwrap();
        assert_eq!(usage.counts().unique_signed_objects, 3);
        assert_eq!(usage.counts().unique_signatures, 4);
        assert_eq!(usage.counts().max_chain_depth, 1);
        let payload = policy_entry_payload_usage(&entry).unwrap();
        assert_eq!(payload.counts().unique_signed_objects, 1);
        assert_eq!(payload.counts().unique_signatures, 2);
    }

    #[test]
    fn payload_meter_counts_committed_objects_without_reading_policy_dependencies() {
        let fixture = Fixture::new();
        let proposal = Signed::sign(next(&fixture), &fixture.root).unwrap();
        let authority = Identity::from_secret_hex(&hex::encode([3; 32])).unwrap();
        let previous = proposal.payload.previous.clone().unwrap();
        let commit = Signed::sign(
            PolicyCommit {
                version: AUTH_VERSION,
                previous: previous.clone(),
                proposal_hash: proposal.hash().unwrap(),
                accepted_at_ms: 1000,
                issued_at_ms: 1000,
                expires_at_ms: 2000,
                nonce: Nonce::from_bytes([70; 32]),
                dependencies: vec![previous.clone()],
            },
            &authority,
        )
        .unwrap();
        let receipt = Signed::sign(
            FreshnessReceipt {
                version: AUTH_VERSION,
                current: previous,
                challenge_hash: commit.challenge_hash().unwrap(),
                issued_at_ms: 1000,
                expires_at_ms: 2000,
            },
            &authority,
        )
        .unwrap();
        let entry = JournalEntry::AccountCommit(Committed {
            proposal,
            commit,
            receipts: vec![receipt],
        });
        let scope = SignatureVerification::new();
        let payload = scope.run(|| policy_entry_payload_usage(&entry)).unwrap();
        assert_eq!(scope.counts().actual_verifications, 0);
        assert_eq!(payload.counts().unique_signed_objects, 3);
        assert_eq!(payload.counts().unique_signatures, 3);
        let transition = policy_entry_usage(&fixture.state, &entry).unwrap();
        assert_eq!(transition.counts().unique_signed_objects, 6);
        assert_eq!(transition.counts().unique_signatures, 6);
    }

    #[test]
    fn cached_certificates_still_exhaust_the_object_or_signature_budget() {
        let fixture = Fixture::new();
        let mut state = fixture.state.clone();
        let mut document = next(&fixture);
        let source = state.devices.values().next().unwrap().payload.clone();
        for byte in 30..61 {
            let identity = Identity::from_secret_hex(&hex::encode([byte; 32])).unwrap();
            let key = PublicKey::of(&identity);
            let mut certificate = source.clone();
            certificate.subject = Actor::Service(ServiceId::from_key(&key));
            certificate.device = DeviceId::from_key(&key);
            certificate.public_key = key;
            certificate.uses = vec![DeviceUse::Noise, DeviceUse::ServiceInvocation];
            certificate.nonce = Nonce::from_bytes([byte; 32]);
            certificate.prove_possession(&identity).unwrap();
            let certificate = Signed::sign(certificate, &fixture.root).unwrap();
            let hash = certificate.hash().unwrap();
            state.devices.insert(hash.clone(), certificate);
            document.devices.push(hash);
        }
        document.devices.sort();
        let entry = JournalEntry::Account(Signed::sign(document, &fixture.root).unwrap());
        assert!(matches!(
            policy_entry_usage(&state, &entry),
            Err(AuthorizationError::BudgetExceeded)
        ));
    }

    #[test]
    fn root_registration_meters_cached_grant_metadata_after_its_issuer_is_revoked() {
        let mut fixture = Fixture::new();
        let space = SpaceId::personal(&fixture.account);
        let parent_hash = fixture.state.space(&space).unwrap().grants[0].clone();
        let mut grant = fixture.state.grants[&parent_hash].payload.clone();
        grant.parent = Some(parent_hash);
        grant.remaining_delegations -= 1;
        grant.issued_under = fixture
            .state
            .head(&PolicyDomain::Space(space.clone()))
            .unwrap()
            .clone();
        grant.nonce = Nonce::from_bytes([71; 32]);
        let user = Identity::from_secret_hex(&hex::encode([4; 32])).unwrap();
        let child = fixture
            .state
            .install_grant(Signed::sign(grant, &user).unwrap(), 1000)
            .unwrap();
        let device = DeviceId::from_key(&PublicKey::of(&user));
        let mut account = next(&fixture);
        account
            .devices
            .retain(|hash| fixture.state.devices[hash].payload.device != device);
        account.revoked_devices.push(device.clone());
        account.revoked_devices.sort();
        fixture
            .state
            .install_account(Signed::sign(account, &fixture.root).unwrap())
            .unwrap();
        assert!(fixture.state.find_device(&device).is_err());
        let mut document = fixture.state.space(&space).unwrap().clone();
        document.previous = fixture.state.head(&PolicyDomain::Space(space)).cloned();
        document.sequence += 1;
        document.grants.push(child);
        document.grants.sort();
        let proposal = Signed::sign(document, &fixture.root).unwrap();
        let entry = JournalEntry::Space(proposal.clone());
        let scope = SignatureVerification::new();
        let usage = scope
            .run(|| policy_entry_usage(&fixture.state, &entry))
            .unwrap();
        assert_eq!(scope.counts().actual_verifications, 0);
        assert_eq!(usage.counts().unique_signed_objects, 6);
        fixture.state.install_space(proposal).unwrap();
    }
}
