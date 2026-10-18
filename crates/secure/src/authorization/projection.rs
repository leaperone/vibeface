use super::*;

impl VerifiedPolicySet {
    #[allow(clippy::too_many_arguments)]
    pub fn client_authorization_projection(
        &self,
        pins: &[PolicyPin],
        journal: &[JournalEntry],
        account: &AccountId,
        space: &SpaceId,
        key: &PublicKey,
        grant_leaf: &ObjectHash,
        now_ms: u64,
    ) -> Result<ClientAuthorizationBundle> {
        require(
            now_ms <= 9_007_199_254_740_991,
            "invalid projection timestamp",
        )?;
        let certificate = self.find_device(&DeviceId::from_key(key))?;
        require(
            certificate.payload.public_key == *key
                && certificate.payload.subject == Actor::Account(account.clone()),
            "client device belongs to another account",
        )?;
        let mut budget = Budget::default();
        let mut dependencies = BTreeMap::new();
        self.validate_certificate(
            certificate,
            now_ms,
            &mut budget,
            &mut dependencies,
            &mut BTreeSet::new(),
        )?;
        let path = self.grant_chain(Some(grant_leaf))?;
        self.verify_grant_path(&path, now_ms, &mut budget, &mut dependencies, true)?;
        let leaf = &path
            .last()
            .ok_or(AuthorizationError::InvalidProof("missing client grant"))?
            .payload;
        require(
            leaf.owner_space == *space
                && self.matches_grantee(
                    &leaf.grantee,
                    &Actor::Account(account.clone()),
                    now_ms,
                    &mut budget,
                    &mut dependencies,
                )?,
            "client grant does not authorize this account and space",
        )?;
        match &leaf.scope {
            ResourceScope::Exact(resource) => {
                let record = self.resource(&resource.space, &resource.resource).ok_or(
                    AuthorizationError::InvalidProof("missing client resource record"),
                )?;
                budget.verify(record)?;
                require(
                    record.payload.host == leaf.audience.service,
                    "client resource audience mismatch",
                )?;
            }
            ResourceScope::WholeSpace(id) => {
                let document = self
                    .space(id)
                    .ok_or(AuthorizationError::InvalidProof("missing resource space"))?;
                for hash in &document.resources {
                    if let Some(record) = self.resources.get(hash)
                        && record.payload.kind == ResourceKind::Project
                        && record.payload.space == *id
                        && record.payload.host == leaf.audience.service
                    {
                        budget.verify(record)?;
                    }
                }
            }
        }
        let target = self.find_device(&leaf.audience.device)?;
        require(
            target.payload.subject == Actor::Service(leaf.audience.service.clone()),
            "client target service mismatch",
        )?;
        self.validate_certificate(
            target,
            now_ms,
            &mut budget,
            &mut dependencies,
            &mut BTreeSet::new(),
        )?;
        self.expand_authorities(now_ms, &mut budget, &mut dependencies)?;

        let mut objects = budget.objects;
        self.include_account_history_certificates(account, &mut objects)?;

        let mut scoped_pins: Vec<_> = pins
            .iter()
            .filter(|pin| dependencies.contains_key(&pin.domain))
            .cloned()
            .collect();
        scoped_pins.sort_by(|a, b| a.domain.cmp(&b.domain));
        let mut projected = Vec::new();
        let mut included_objects = BTreeSet::new();
        for entry in journal {
            let include = match entry {
                JournalEntry::AccountGenesis(value) | JournalEntry::Account(value) => {
                    dependencies.contains_key(&PolicyDomain::Account(value.payload.account.clone()))
                }
                JournalEntry::SpaceGenesis(value) | JournalEntry::Space(value) => {
                    dependencies.contains_key(&PolicyDomain::Space(value.payload.space.clone()))
                }
                JournalEntry::AccountCommit(value) => dependencies.contains_key(
                    &PolicyDomain::Account(value.proposal.payload.account.clone()),
                ),
                JournalEntry::SpaceCommit(value) => dependencies
                    .contains_key(&PolicyDomain::Space(value.proposal.payload.space.clone())),
                JournalEntry::Device(value) => objects.contains(&value.hash()?),
                JournalEntry::Resource(value) => objects.contains(&value.hash()?),
                JournalEntry::Membership(value) => objects.contains(&value.hash()?),
                JournalEntry::Grant(value) => objects.contains(&value.hash()?),
                JournalEntry::Authenticator(_) => false,
            };
            if include {
                let object = match entry {
                    JournalEntry::Device(value) => Some(value.hash()?),
                    JournalEntry::Resource(value) => Some(value.hash()?),
                    JournalEntry::Membership(value) => Some(value.hash()?),
                    JournalEntry::Grant(value) => Some(value.hash()?),
                    _ => None,
                };
                included_objects.extend(object);
                projected.push(entry.clone());
            }
        }
        require(
            objects.is_subset(&included_objects),
            "missing scoped proof object",
        )?;
        let bundle = ClientAuthorizationBundle {
            journal: projected,
            grant_leaf: grant_leaf.clone(),
        };
        let encoded_bundle = serde_json::to_string(&bundle)
            .map_err(|_| AuthorizationError::InvalidProof("projection encoding failed"))?;
        let encoded_pins = serde_json::to_string(&scoped_pins)
            .map_err(|_| AuthorizationError::InvalidProof("projection pin encoding failed"))?;
        let verified = verify_client_authorization(
            &encoded_bundle,
            &encoded_pins,
            account.as_str(),
            space.as_str(),
            key.as_str(),
            now_ms as f64,
        )
        .map_err(|_| {
            AuthorizationError::InvalidProof("policy history requires additional scoped proof")
        })?;
        require(
            verified.heads == dependencies.into_values().collect::<Vec<_>>(),
            "client projection does not match current policy heads",
        )?;
        Ok(bundle)
    }

    fn include_account_history_certificates(
        &self,
        account: &AccountId,
        objects: &mut BTreeSet<ObjectHash>,
    ) -> Result<()> {
        let history = self
            .account_history(account)
            .ok_or(AuthorizationError::InvalidProof(
                "missing client account history",
            ))?;
        for pair in history.windows(2) {
            let previous = &pair[0].payload;
            let next = &pair[1];
            let committed = self.commits.contains_key(&next.hash()?);
            let daily = next.issuer != previous.current_root
                && !previous.recovery.keys.contains(&next.issuer);
            for hash in &previous.devices {
                let required = !next.payload.devices.contains(hash)
                    || (committed
                        && self.devices.get(hash).is_some_and(|certificate| {
                            certificate.payload.public_key == next.issuer
                                || certificate.payload.public_key == previous.authority.public_key
                        }));
                if required {
                    objects.insert(hash.clone());
                }
                if !next.payload.devices.contains(hash)
                    && let Some(old) = self.devices.get(hash)
                    && !next.payload.revoked_devices.contains(&old.payload.device)
                {
                    for replacement in &next.payload.devices {
                        if self.devices.get(replacement).is_some_and(|certificate| {
                            certificate.payload.device == old.payload.device
                        }) {
                            objects.insert(replacement.clone());
                        }
                    }
                }
            }
            if committed && daily {
                objects.extend(
                    next.payload
                        .devices
                        .iter()
                        .filter(|hash| !previous.devices.contains(hash))
                        .cloned(),
                );
            }
        }
        loop {
            let mut issuers = BTreeSet::new();
            for hash in objects.iter() {
                let Some(certificate) = self.devices.get(hash) else {
                    continue;
                };
                if certificate.payload.owner != PolicyDomain::Account(account.clone())
                    || certificate.issuer
                        == self.historical_root(&certificate.payload.issued_under)?
                {
                    continue;
                }
                for (hash, issuer) in &self.devices {
                    if issuer.payload.owner == certificate.payload.owner
                        && issuer.payload.public_key == certificate.issuer
                    {
                        issuers.insert(hash.clone());
                    }
                }
            }
            let before = objects.len();
            objects.extend(issuers);
            if objects.len() == before {
                return Ok(());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: u64 = 10_000;

    fn key(byte: u8) -> Identity {
        Identity::from_secret_hex(&hex::encode([byte; 32])).unwrap()
    }

    struct Fixture {
        state: VerifiedPolicySet,
        pins: Vec<PolicyPin>,
        journal: Vec<JournalEntry>,
        root: Identity,
        user: Identity,
        node: Identity,
        account: AccountId,
        space: SpaceId,
        resource: ResourceRef,
        grant: ObjectHash,
    }

    impl Fixture {
        fn new(offset: u8) -> Self {
            let root = key(offset + 1);
            let recovery = key(offset + 2);
            let user = key(offset + 3);
            let node = key(offset + 4);
            let nonce = Nonce::from_bytes([offset + 5; 32]);
            let claim = bootstrap::create_node_claim(
                &node,
                nonce.as_str(),
                NOW as f64,
                (NOW + 10_000) as f64,
            )
            .unwrap();
            let claim = bootstrap::verify_node_claim(
                &claim,
                &node.public_key_hex(),
                nonce.as_str(),
                NOW as f64,
            )
            .unwrap();
            let prepared = bootstrap::prepare_account_creation(
                &root,
                &recovery,
                &user,
                &claim,
                (NOW + 1_000_000) as f64,
                NOW as f64,
            )
            .unwrap();
            let possession = bootstrap::sign_node_possession(
                &prepared.node_request_json().unwrap(),
                &node,
                nonce.as_str(),
                NOW as f64,
            )
            .unwrap();
            let created =
                bootstrap::finish_account_creation(&prepared, &root, &possession, NOW as f64)
                    .unwrap();
            let bundle: ClientAuthorizationBundle =
                serde_json::from_str(&created.bundle_json().unwrap()).unwrap();
            let pins: Vec<PolicyPin> = serde_json::from_str(&created.pins_json().unwrap()).unwrap();
            let scope: ClientScope = serde_json::from_str(&created.scope_json().unwrap()).unwrap();
            let state = replay_policies(&pins, &bundle.journal).unwrap();
            let resource = bundle
                .journal
                .iter()
                .find_map(|entry| match entry {
                    JournalEntry::Resource(value) => Some(ResourceRef {
                        space: value.payload.space.clone(),
                        resource: value.payload.resource.clone(),
                    }),
                    _ => None,
                })
                .unwrap();
            Self {
                state,
                pins,
                journal: bundle.journal,
                root,
                user,
                node,
                account: scope.account_id,
                space: scope.space_id,
                resource,
                grant: bundle.grant_leaf,
            }
        }

        fn project(&self) -> Result<ClientAuthorizationBundle> {
            self.state.client_authorization_projection(
                &self.pins,
                &self.journal,
                &self.account,
                &self.space,
                &PublicKey::of(&self.user),
                &self.grant,
                NOW,
            )
        }

        fn verify(&self, bundle: &ClientAuthorizationBundle) -> VerifiedClientAuthorization {
            let domains: BTreeSet<_> = bundle
                .journal
                .iter()
                .filter_map(|entry| match entry {
                    JournalEntry::AccountGenesis(value) => {
                        Some(PolicyDomain::Account(value.payload.account.clone()))
                    }
                    JournalEntry::SpaceGenesis(value) => {
                        Some(PolicyDomain::Space(value.payload.space.clone()))
                    }
                    _ => None,
                })
                .collect();
            let pins: Vec<_> = self
                .pins
                .iter()
                .filter(|pin| domains.contains(&pin.domain))
                .cloned()
                .collect();
            verify_client_authorization(
                &serde_json::to_string(bundle).unwrap(),
                &serde_json::to_string(&pins).unwrap(),
                self.account.as_str(),
                self.space.as_str(),
                &self.user.public_key_hex(),
                NOW as f64,
            )
            .unwrap()
        }

        fn update_space(&mut self, change: impl FnOnce(&mut SpaceDocument)) {
            let mut document = self.state.space(&self.space).unwrap().clone();
            document.previous = self
                .state
                .head(&PolicyDomain::Space(self.space.clone()))
                .cloned();
            document.sequence += 1;
            change(&mut document);
            document.resources.sort();
            document.grants.sort();
            document.memberships.sort();
            let signed = Signed::sign(document, &self.root).unwrap();
            self.state.install_space(signed.clone()).unwrap();
            self.journal.push(JournalEntry::Space(signed));
        }

        fn add_resource(&mut self, byte: u8, host: ServiceId) -> ResourceId {
            let resource = ResourceId::from_bytes([byte; 32]);
            let signed = Signed::sign(
                ResourceRecord {
                    version: AUTH_VERSION,
                    space: self.space.clone(),
                    resource: resource.clone(),
                    kind: ResourceKind::Project,
                    host,
                    issued_under: self
                        .state
                        .head(&PolicyDomain::Space(self.space.clone()))
                        .unwrap()
                        .clone(),
                    nonce: Nonce::from_bytes([byte; 32]),
                },
                &self.root,
            )
            .unwrap();
            let hash = self.state.install_resource(signed.clone()).unwrap();
            self.journal.push(JournalEntry::Resource(signed));
            self.update_space(|document| document.resources.push(hash));
            resource
        }

        fn grant(&mut self, scope: ResourceScope, grantee: Grantee) {
            let signed = Signed::sign(
                Grant {
                    execution_source: None,
                    version: AUTH_VERSION,
                    owner_space: self.space.clone(),
                    scope,
                    grantee,
                    actions: vec![Action::ConversationRead],
                    audience: Audience {
                        service: ServiceId::from_key(&PublicKey::of(&self.node)),
                        device: DeviceId::from_key(&PublicKey::of(&self.node)),
                    },
                    constraints: GrantConstraints::default(),
                    parent: None,
                    remaining_delegations: 0,
                    issued_under: self
                        .state
                        .head(&PolicyDomain::Space(self.space.clone()))
                        .unwrap()
                        .clone(),
                    not_before_ms: NOW,
                    expires_at_ms: NOW + 1_000_000,
                    nonce: Nonce::from_bytes([91; 32]),
                },
                &self.root,
            )
            .unwrap();
            let hash = self.state.install_grant(signed.clone(), NOW).unwrap();
            self.journal.push(JournalEntry::Grant(signed));
            self.update_space(|document| document.grants.push(hash.clone()));
            self.grant = hash;
        }

        fn membership(&mut self, account: AccountId, nonce: u8) {
            let signed = Signed::sign(
                Membership {
                    version: AUTH_VERSION,
                    space: self.space.clone(),
                    subject: Actor::Account(account),
                    roles: vec![SpaceRole::Viewer],
                    issued_under: self
                        .state
                        .head(&PolicyDomain::Space(self.space.clone()))
                        .unwrap()
                        .clone(),
                    not_before_ms: NOW,
                    expires_at_ms: NOW + 1_000_000,
                    nonce: Nonce::from_bytes([nonce; 32]),
                },
                &self.root,
            )
            .unwrap();
            let hash = self.state.install_membership(signed.clone(), NOW).unwrap();
            self.journal.push(JournalEntry::Membership(signed));
            self.update_space(|document| document.memberships.push(hash));
        }

        fn append_network(&mut self, other: Self) {
            self.state.accounts.extend(other.state.accounts);
            self.state.spaces.extend(other.state.spaces);
            self.state.devices.extend(other.state.devices);
            self.state.memberships.extend(other.state.memberships);
            self.state.grants.extend(other.state.grants);
            self.state.resources.extend(other.state.resources);
            self.state.roots.extend(other.state.roots);
            self.state.commits.extend(other.state.commits);
            self.pins.extend(other.pins);
            self.pins.sort_by(|a, b| a.domain.cmp(&b.domain));
            self.journal.extend(other.journal);
        }
    }

    #[test]
    fn projection_excludes_other_accounts_even_when_server_has_more_than_ten_domains() {
        let mut fixture = Fixture::new(0);
        let mut unrelated = Vec::new();
        for offset in [10, 20, 30, 40, 50] {
            let other = Fixture::new(offset);
            unrelated.push((
                other.account.clone(),
                PublicKey::of(&other.user),
                other.resource.resource.clone(),
            ));
            fixture.append_network(other);
        }
        assert!(fixture.pins.len() > MAX_POLICY_DOMAINS);
        let projected = fixture.project().unwrap();
        let verified = fixture.verify(&projected);
        assert_eq!(verified.scope.account_id, fixture.account);
        assert_eq!(verified.resources().len(), 1);
        let encoded = serde_json::to_string(&projected).unwrap();
        for (account, key, resource) in unrelated {
            assert!(!encoded.contains(account.as_str()));
            assert!(!encoded.contains(key.as_str()));
            assert!(!encoded.contains(resource.as_str()));
        }
    }

    #[test]
    fn exact_resource_projection_keeps_other_resource_ids_and_hosts_opaque() {
        let mut fixture = Fixture::new(0);
        let hidden_host = ServiceId::from_key(&PublicKey::of(&key(99)));
        let hidden = fixture.add_resource(41, hidden_host.clone());
        fixture.grant(
            ResourceScope::Exact(fixture.resource.clone()),
            Grantee::Actor(Actor::Account(fixture.account.clone())),
        );
        let projected = fixture.project().unwrap();
        let verified = fixture.verify(&projected);
        assert_eq!(verified.resources().len(), 1);
        assert_eq!(
            verified.resources()[0].payload.resource,
            fixture.resource.resource
        );
        let encoded = serde_json::to_string(&projected).unwrap();
        assert!(!encoded.contains(hidden.as_str()));
        assert!(!encoded.contains(hidden_host.as_str()));
        assert_eq!(
            projected
                .journal
                .iter()
                .filter(|entry| matches!(entry, JournalEntry::Grant(_)))
                .count(),
            1
        );
    }

    #[test]
    fn whole_space_projection_contains_all_visible_host_resources() {
        let mut fixture = Fixture::new(0);
        let visible = fixture.add_resource(41, ServiceId::from_key(&PublicKey::of(&fixture.node)));
        let other = Fixture::new(10);
        let foreign = other
            .journal
            .iter()
            .find_map(|entry| match entry {
                JournalEntry::Resource(value) => Some(value.clone()),
                _ => None,
            })
            .unwrap();
        let foreign_hash = foreign.hash().unwrap();
        fixture.append_network(other);
        let mut invalid = fixture.state.space(&fixture.space).unwrap().clone();
        invalid.previous = fixture
            .state
            .head(&PolicyDomain::Space(fixture.space.clone()))
            .cloned();
        invalid.sequence += 1;
        invalid.resources.push(foreign_hash);
        invalid.resources.sort();
        assert_eq!(
            fixture
                .state
                .install_space(Signed::sign(invalid, &fixture.root).unwrap())
                .unwrap_err(),
            AuthorizationError::InvalidProof("invalid registered resource")
        );
        let projected = fixture.project().unwrap();
        let verified = fixture.verify(&projected);
        assert_eq!(verified.resources().len(), 2);
        assert!(
            verified
                .resources()
                .iter()
                .any(|record| record.payload.resource == visible)
        );
        assert!(
            !serde_json::to_string(&projected)
                .unwrap()
                .contains(foreign.payload.resource.as_str())
        );
    }

    #[test]
    fn delegated_exact_projection_keeps_the_verified_ancestor_chain() {
        let mut fixture = Fixture::new(0);
        let mut child = fixture.state.grant(&fixture.grant).unwrap().payload.clone();
        child.parent = Some(fixture.grant.clone());
        child.scope = ResourceScope::Exact(fixture.resource.clone());
        child.remaining_delegations -= 1;
        child.issued_under = fixture
            .state
            .head(&PolicyDomain::Space(fixture.space.clone()))
            .unwrap()
            .clone();
        child.nonce = Nonce::from_bytes([73; 32]);
        let signed = Signed::sign(child, &fixture.user).unwrap();
        let hash = fixture.state.install_grant(signed.clone(), NOW).unwrap();
        fixture.journal.push(JournalEntry::Grant(signed));
        fixture.update_space(|document| document.grants.push(hash.clone()));
        fixture.grant = hash;
        let projected = fixture.project().unwrap();
        assert_eq!(
            projected
                .journal
                .iter()
                .filter(|entry| matches!(entry, JournalEntry::Grant(_)))
                .count(),
            2
        );
        assert_eq!(fixture.verify(&projected).grant_leaf(), &fixture.grant);
        let revoked = fixture.grant.clone();
        fixture.update_space(|document| document.grants.retain(|hash| hash != &revoked));
        assert_eq!(fixture.project().unwrap_err(), AuthorizationError::Revoked);
    }

    #[test]
    fn selector_projection_exports_only_the_membership_used_by_the_client() {
        let mut fixture = Fixture::new(0);
        let other = AccountId::from_root(&PublicKey::of(&key(77)));
        fixture.membership(fixture.account.clone(), 61);
        fixture.membership(other.clone(), 62);
        let genesis = fixture.state.space_history(&fixture.space).unwrap()[0]
            .hash()
            .unwrap();
        fixture.grant(
            ResourceScope::Exact(fixture.resource.clone()),
            Grantee::MembersOf(MembersOf {
                space: fixture.space.clone(),
                pinned_genesis: genesis,
                kind: MemberKind::Account,
                roles_any: vec![SpaceRole::Viewer],
            }),
        );
        let projected = fixture.project().unwrap();
        fixture.verify(&projected);
        let members: Vec<_> = projected
            .journal
            .iter()
            .filter_map(|entry| match entry {
                JournalEntry::Membership(value) => Some(&value.payload.subject),
                _ => None,
            })
            .collect();
        assert_eq!(members, vec![&Actor::Account(fixture.account.clone())]);
        assert!(
            !serde_json::to_string(&projected)
                .unwrap()
                .contains(other.as_str())
        );
    }

    #[test]
    fn projection_rejects_tampered_signed_objects_and_stale_or_missing_history() {
        let mut fixture = Fixture::new(0);
        let original = fixture.journal.clone();
        if let JournalEntry::Device(value) = fixture
            .journal
            .iter_mut()
            .find(|entry| matches!(entry, JournalEntry::Device(_)))
            .unwrap()
        {
            value.signature = Signed::sign(value.payload.clone(), &key(99))
                .unwrap()
                .signature;
        }
        assert!(fixture.project().is_err());
        fixture.journal = original.clone();
        fixture
            .journal
            .retain(|entry| !matches!(entry, JournalEntry::Resource(_)));
        assert!(fixture.project().is_err());
        fixture.journal = original;
        fixture.update_space(|_| {});
        fixture.journal.pop();
        assert!(fixture.project().is_err());
    }

    #[test]
    fn daily_enrollment_keeps_owner_login_working_and_cross_account_proof_fails_closed() {
        let mut fixture = Fixture::new(0);
        let new_device = key(55);
        let previous = fixture
            .state
            .head(&PolicyDomain::Account(fixture.account.clone()))
            .unwrap()
            .clone();
        let mut certificate = DeviceCertificate {
            version: AUTH_VERSION,
            owner: previous.domain.clone(),
            subject: Actor::Account(fixture.account.clone()),
            device: DeviceId::from_key(&PublicKey::of(&new_device)),
            public_key: PublicKey::of(&new_device),
            uses: vec![DeviceUse::Noise, DeviceUse::AccountInvocation],
            possession: None,
            enrollment_grant: None,
            issued_under: previous.clone(),
            nonce: Nonce::from_bytes([66; 32]),
            not_before_ms: NOW,
            expires_at_ms: NOW + 100_000,
        };
        certificate.prove_possession(&new_device).unwrap();
        let certificate = Signed::sign(certificate, &fixture.user).unwrap();
        let hash = fixture
            .state
            .install_device(certificate.clone(), NOW)
            .unwrap();
        fixture.journal.push(JournalEntry::Device(certificate));
        let mut document = fixture.state.account(&fixture.account).unwrap().clone();
        document.previous = Some(previous.clone());
        document.sequence += 1;
        document.devices.push(hash);
        document.devices.sort();
        let proposal = Signed::sign(document, &fixture.user).unwrap();
        let commit = Signed::sign(
            PolicyCommit {
                version: AUTH_VERSION,
                previous: previous.clone(),
                proposal_hash: proposal.hash().unwrap(),
                accepted_at_ms: NOW,
                issued_at_ms: NOW,
                expires_at_ms: NOW + 10_000,
                nonce: Nonce::from_bytes([67; 32]),
                dependencies: vec![previous.clone()],
            },
            &fixture.node,
        )
        .unwrap();
        let receipt = Signed::sign(
            FreshnessReceipt {
                version: AUTH_VERSION,
                currents: vec![previous],
                challenge_hash: commit.challenge_hash().unwrap(),
                issued_at_ms: NOW,
                expires_at_ms: NOW + 10_000,
            },
            &fixture.node,
        )
        .unwrap();
        let change = Committed {
            proposal,
            commit,
            receipts: vec![receipt],
        };
        fixture
            .state
            .install_account_committed(change.clone(), NOW)
            .unwrap();
        fixture.journal.push(JournalEntry::AccountCommit(change));
        let owner_projection = fixture.project().unwrap();
        fixture.verify(&owner_projection);
        assert!(
            serde_json::to_string(&owner_projection)
                .unwrap()
                .contains(&new_device.public_key_hex())
        );
        let mut consumer = Fixture::new(10);
        fixture.grant(
            ResourceScope::Exact(fixture.resource.clone()),
            Grantee::Actor(Actor::Account(consumer.account.clone())),
        );
        consumer.space = fixture.space.clone();
        consumer.resource = fixture.resource.clone();
        consumer.grant = fixture.grant.clone();
        consumer.append_network(fixture);
        assert_eq!(
            consumer.project().unwrap_err(),
            AuthorizationError::InvalidProof("policy history requires additional scoped proof")
        );
    }
}
