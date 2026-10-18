use super::*;

#[derive(Clone, Debug)]
pub(crate) struct BoundedPolicyImport {
    state: VerifiedPolicySet,
    pins: Vec<PolicyPin>,
    required_heads: BTreeSet<PolicyStamp>,
    seen_heads: BTreeSet<PolicyStamp>,
    journal: Vec<JournalEntry>,
    entry_index: BTreeMap<ObjectHash, usize>,
    nonces: BTreeMap<(PolicyDomain, Nonce), ObjectHash>,
    authenticator_generations: BTreeSet<(AccountId, u64)>,
}

#[derive(Clone, Debug)]
pub(crate) struct VerifiedPolicySnapshot {
    state: VerifiedPolicySet,
    pins: Vec<PolicyPin>,
    heads: Vec<PolicyStamp>,
    journal: Vec<JournalEntry>,
    entry_index: BTreeMap<ObjectHash, usize>,
    nonces: BTreeMap<(PolicyDomain, Nonce), ObjectHash>,
    authenticator_generations: BTreeSet<(AccountId, u64)>,
}

fn registered_generation(entry: &JournalEntry) -> Option<(AccountId, u64)> {
    let account = match entry {
        JournalEntry::AccountGenesis(value) | JournalEntry::Account(value) => &value.payload,
        JournalEntry::AccountCommit(value) => &value.proposal.payload,
        _ => return None,
    };
    (!account.authenticators.is_empty()).then(|| (account.account.clone(), account.root_generation))
}

fn allowed(pins: &[PolicyPin], domain: &PolicyDomain) -> Result<()> {
    require(
        pins.iter().any(|pin| &pin.domain == domain),
        "policy import requires an independently pinned dependency domain",
    )
}

fn witness(state: &VerifiedPolicySet, pins: &[PolicyPin], stamp: &PolicyStamp) -> Result<()> {
    allowed(pins, &stamp.domain)?;
    require(
        state.roots.contains_key(stamp),
        "policy import is missing an exact issued-under witness",
    )
}

fn entry_domain(entry: &JournalEntry) -> PolicyDomain {
    match entry {
        JournalEntry::AccountGenesis(value) | JournalEntry::Account(value) => {
            PolicyDomain::Account(value.payload.account.clone())
        }
        JournalEntry::AccountCommit(value) => {
            PolicyDomain::Account(value.proposal.payload.account.clone())
        }
        JournalEntry::SpaceGenesis(value) | JournalEntry::Space(value) => {
            PolicyDomain::Space(value.payload.space.clone())
        }
        JournalEntry::SpaceCommit(value) => {
            PolicyDomain::Space(value.proposal.payload.space.clone())
        }
        JournalEntry::Device(value) => value.payload.owner.clone(),
        JournalEntry::Resource(value) => PolicyDomain::Space(value.payload.space.clone()),
        JournalEntry::Authenticator(value) => PolicyDomain::Account(value.payload.account.clone()),
        JournalEntry::Membership(value) => PolicyDomain::Space(value.payload.space.clone()),
        JournalEntry::Grant(value) => PolicyDomain::Space(value.payload.owner_space.clone()),
    }
}

pub(crate) fn entry_hash(entry: &JournalEntry) -> Result<ObjectHash> {
    match entry {
        JournalEntry::AccountGenesis(value) | JournalEntry::Account(value) => value.hash(),
        JournalEntry::AccountCommit(value) => value.proposal.hash(),
        JournalEntry::SpaceGenesis(value) | JournalEntry::Space(value) => value.hash(),
        JournalEntry::SpaceCommit(value) => value.proposal.hash(),
        JournalEntry::Device(value) => value.hash(),
        JournalEntry::Resource(value) => value.hash(),
        JournalEntry::Authenticator(value) => value.hash(),
        JournalEntry::Membership(value) => value.hash(),
        JournalEntry::Grant(value) => value.hash(),
    }
}

fn entry_commit(entry: &JournalEntry) -> Option<&Signed<PolicyCommit>> {
    match entry {
        JournalEntry::AccountCommit(value) => Some(&value.commit),
        JournalEntry::SpaceCommit(value) => Some(&value.commit),
        _ => None,
    }
}

fn device_reference(
    state: &VerifiedPolicySet,
    pins: &[PolicyPin],
    hash: &ObjectHash,
    domain: &PolicyDomain,
    embedded: Option<&Signed<DeviceCertificate>>,
) -> Result<()> {
    let certificate = if let Some(certificate) = state.devices.get(hash) {
        certificate
    } else {
        let certificate = embedded.ok_or(AuthorizationError::InvalidProof(
            "policy import is missing a referenced device certificate",
        ))?;
        require(
            certificate.hash()? == *hash && &certificate.payload.owner == domain,
            "embedded certificate differs from the policy reference",
        )?;
        certificate
    };
    allowed(pins, &certificate.payload.owner)?;
    witness(state, pins, &certificate.payload.issued_under)?;
    require(
        match domain {
            PolicyDomain::Account(_) => &certificate.payload.owner == domain,
            PolicyDomain::Space(_) => matches!(certificate.payload.owner, PolicyDomain::Account(_)),
        },
        "policy import has a cross-domain device reference",
    )
}

fn assertion_reference(
    state: &VerifiedPolicySet,
    pins: &[PolicyPin],
    proof: &crate::webauthn::PasskeyAssertionProof,
) -> Result<()> {
    witness(state, pins, &proof.record.payload.issued_under)?;
    for head in &proof.pending.envelope().expected_heads {
        witness(state, pins, head)?;
    }
    require(
        state.authenticators.get(&proof.record.hash()?) == Some(&proof.record),
        "policy import is missing the registered Passkey record",
    )
}

fn account_references(
    state: &VerifiedPolicySet,
    pins: &[PolicyPin],
    document: &AccountDocument,
    embedded: Option<&Signed<DeviceCertificate>>,
) -> Result<()> {
    let domain = PolicyDomain::Account(document.account.clone());
    for hash in &document.devices {
        device_reference(state, pins, hash, &domain, embedded)?;
    }
    for hash in &document.authenticators {
        let record = state
            .authenticators
            .get(hash)
            .ok_or(AuthorizationError::InvalidProof(
                "policy import is missing a referenced authenticator",
            ))?;
        require(
            record.payload.account == document.account,
            "cross-account authenticator reference",
        )?;
        witness(state, pins, &record.payload.issued_under)?;
    }
    Ok(())
}

fn space_references(
    state: &VerifiedPolicySet,
    pins: &[PolicyPin],
    document: &SpaceDocument,
) -> Result<()> {
    let domain = PolicyDomain::Space(document.space.clone());
    if let SpaceRoot::Personal { owner, .. } = &document.root {
        allowed(pins, &PolicyDomain::Account(owner.clone()))?;
        require(
            state.account(owner).is_some(),
            "personal-space owner policy is missing",
        )?;
    }
    for hash in &document.devices {
        device_reference(state, pins, hash, &domain, None)?;
    }
    for hash in &document.resources {
        let value = state
            .resources
            .get(hash)
            .ok_or(AuthorizationError::InvalidProof(
                "policy import is missing a referenced resource",
            ))?;
        require(
            value.payload.space == document.space,
            "cross-space resource reference",
        )?;
        witness(state, pins, &value.payload.issued_under)?;
    }
    for hash in &document.memberships {
        let value = state
            .memberships
            .get(hash)
            .ok_or(AuthorizationError::InvalidProof(
                "policy import is missing a referenced membership",
            ))?;
        require(
            value.payload.space == document.space,
            "cross-space membership reference",
        )?;
        witness(state, pins, &value.payload.issued_under)?;
    }
    for hash in &document.grants {
        let value = state
            .grants
            .get(hash)
            .ok_or(AuthorizationError::InvalidProof(
                "policy import is missing a referenced grant",
            ))?;
        require(
            value.payload.owner_space == document.space,
            "cross-space grant reference",
        )?;
        witness(state, pins, &value.payload.issued_under)?;
    }
    if let Some(hash) = &document.governance_grant {
        require(
            state.grants.contains_key(hash),
            "policy import is missing its governance grant",
        )?;
    }
    Ok(())
}

fn account_transition(
    state: &VerifiedPolicySet,
    pins: &[PolicyPin],
    document: &AccountDocument,
) -> Result<()> {
    if let Some(previous) = state.account(&document.account) {
        account_references(state, pins, previous, None)?;
    }
    let changed_proof = document.device_enrollment.as_ref().filter(|proof| {
        state
            .account(&document.account)
            .and_then(|old| old.device_enrollment.as_ref())
            != Some(*proof)
    });
    if let Some(proof) = changed_proof {
        for head in &proof.intent.payload.expected_heads {
            witness(state, pins, head)?;
        }
        assertion_reference(state, pins, &proof.assertion)?;
        require(
            document.devices.contains(&proof.certificate.hash()?),
            "embedded enrollment certificate is not registered by its policy",
        )?;
    }
    if let Some(approval) = &document.authenticator_approval {
        for head in &approval.intent.payload.expected_heads {
            witness(state, pins, head)?;
        }
        assertion_reference(state, pins, &approval.assertion)?;
    }
    account_references(
        state,
        pins,
        document,
        changed_proof.map(|proof| &proof.certificate),
    )
}

fn space_transition(
    state: &VerifiedPolicySet,
    pins: &[PolicyPin],
    document: &SpaceDocument,
) -> Result<()> {
    if let Some(previous) = state.space(&document.space) {
        space_references(state, pins, previous)?;
    }
    space_references(state, pins, document)
}

fn check_entry_dependencies(
    state: &VerifiedPolicySet,
    pins: &[PolicyPin],
    entry: &JournalEntry,
) -> Result<()> {
    allowed(pins, &entry_domain(entry))?;
    if let Some(commit) = entry_commit(entry) {
        witness(state, pins, &commit.payload.previous)?;
        for head in &commit.payload.dependencies {
            witness(state, pins, head)?;
        }
    }
    match entry {
        JournalEntry::AccountGenesis(value) | JournalEntry::Account(value) => {
            account_transition(state, pins, &value.payload)?;
        }
        JournalEntry::AccountCommit(value) => {
            account_transition(state, pins, &value.proposal.payload)?;
            for receipt in &value.receipts {
                witness(state, pins, &receipt.payload.current)?;
            }
        }
        JournalEntry::SpaceGenesis(value) | JournalEntry::Space(value) => {
            space_transition(state, pins, &value.payload)?;
        }
        JournalEntry::SpaceCommit(value) => {
            space_transition(state, pins, &value.proposal.payload)?;
            for receipt in &value.receipts {
                witness(state, pins, &receipt.payload.current)?;
            }
        }
        JournalEntry::Device(value) => {
            witness(state, pins, &value.payload.issued_under)?;
            if let Some(grant) = &value.payload.enrollment_grant {
                require(
                    state.grants.contains_key(grant),
                    "missing device enrollment grant",
                )?;
            }
        }
        JournalEntry::Resource(value) => witness(state, pins, &value.payload.issued_under)?,
        JournalEntry::Authenticator(value) => witness(state, pins, &value.payload.issued_under)?,
        JournalEntry::Membership(value) => witness(state, pins, &value.payload.issued_under)?,
        JournalEntry::Grant(value) => {
            witness(state, pins, &value.payload.issued_under)?;
            if let Some(parent) = &value.payload.parent {
                require(state.grants.contains_key(parent), "missing parent grant")?;
            }
            let scope = match &value.payload.scope {
                ResourceScope::WholeSpace(space) => space,
                ResourceScope::Exact(resource) => &resource.space,
            };
            allowed(pins, &PolicyDomain::Space(scope.clone()))?;
            if let Grantee::MembersOf(selector) = &value.payload.grantee {
                allowed(pins, &PolicyDomain::Space(selector.space.clone()))?;
                let genesis = state
                    .space_history(&selector.space)
                    .and_then(|history| history.first())
                    .ok_or(AuthorizationError::InvalidProof(
                        "missing membership selector genesis",
                    ))?;
                require(
                    genesis.hash()? == selector.pinned_genesis,
                    "selector genesis mismatch",
                )?;
            }
        }
    }
    Ok(())
}

impl BoundedPolicyImport {
    pub(crate) fn state(&self) -> &VerifiedPolicySet {
        &self.state
    }

    pub(crate) fn new(pins: Vec<PolicyPin>, required_heads: Vec<PolicyStamp>) -> Result<Self> {
        require(
            !pins.is_empty()
                && pins.len() <= MAX_POLICY_DOMAINS
                && pins.windows(2).all(|pair| pair[0].domain < pair[1].domain)
                && required_heads.len() <= pins.len()
                && required_heads
                    .windows(2)
                    .all(|pair| pair[0].domain < pair[1].domain),
            "invalid policy import domains or required heads",
        )?;
        let mut required: BTreeSet<_> = required_heads.into_iter().collect();
        for pin in &pins {
            if let Some(minimum) = &pin.minimum_head {
                require(
                    minimum.domain == pin.domain,
                    "policy minimum belongs to another domain",
                )?;
                required.insert(minimum.clone());
            }
        }
        for head in &required {
            allowed(&pins, &head.domain)?;
        }
        Ok(Self {
            state: VerifiedPolicySet::new(),
            pins,
            required_heads: required,
            seen_heads: BTreeSet::new(),
            journal: Vec::new(),
            entry_index: BTreeMap::new(),
            nonces: BTreeMap::new(),
            authenticator_generations: BTreeSet::new(),
        })
    }

    pub(crate) fn apply_entry(&mut self, entry: JournalEntry) -> Result<()> {
        require(
            self.journal.len() < MAX_POLICY_HISTORY * MAX_POLICY_DOMAINS
                && canonical(&entry)?.len() <= MAX_PROOF_BYTES,
            "policy import entry exceeds existing bounds",
        )?;
        let hash = entry_hash(&entry)?;
        require(
            !self.entry_index.contains_key(&hash),
            "duplicate or conflicting policy import entry",
        )?;
        let nonce = entry_commit(&entry).map(|commit| {
            (
                commit.payload.previous.domain.clone(),
                commit.payload.nonce.clone(),
            )
        });
        if let Some(nonce) = &nonce {
            require(
                !self.nonces.contains_key(nonce),
                "policy commit nonce was reused",
            )?;
        }
        check_entry_dependencies(&self.state, &self.pins, &entry)?;
        let mut staged = self.state.clone();
        apply_policy_entry(&mut staged, &self.pins, entry.clone())?;
        match &entry {
            JournalEntry::AccountGenesis(value) | JournalEntry::Account(value) => {
                account_references(&staged, &self.pins, &value.payload, None)?
            }
            JournalEntry::AccountCommit(value) => {
                account_references(&staged, &self.pins, &value.proposal.payload, None)?
            }
            JournalEntry::SpaceGenesis(value) | JournalEntry::Space(value) => {
                space_references(&staged, &self.pins, &value.payload)?
            }
            JournalEntry::SpaceCommit(value) => {
                space_references(&staged, &self.pins, &value.proposal.payload)?
            }
            _ => {}
        }
        if let Some(head) = staged.head(&entry_domain(&entry)) {
            if self.required_heads.contains(head) {
                self.seen_heads.insert(head.clone());
            }
        }
        self.state = staged;
        if let Some(generation) = registered_generation(&entry) {
            self.authenticator_generations.insert(generation);
        }
        if let Some(nonce) = nonce {
            self.nonces.insert(nonce, hash.clone());
        }
        self.entry_index.insert(hash, self.journal.len());
        self.journal.push(entry);
        Ok(())
    }

    pub(crate) fn finish(self, target_heads: &[PolicyStamp]) -> Result<VerifiedPolicySnapshot> {
        require(
            target_heads.len() == self.pins.len()
                && target_heads.iter().zip(&self.pins).all(|(head, pin)| {
                    head.domain == pin.domain && self.state.head(&pin.domain) == Some(head)
                })
                && self.required_heads == self.seen_heads,
            "policy import did not reach its required exact heads",
        )?;
        for space in self.state.spaces.values() {
            self.state.ensure_personal_current(
                &space
                    .history
                    .last()
                    .ok_or(AuthorizationError::PolicyConflict)?
                    .payload,
            )?;
        }
        Ok(VerifiedPolicySnapshot {
            state: self.state,
            pins: self.pins,
            heads: target_heads.to_vec(),
            journal: self.journal,
            entry_index: self.entry_index,
            nonces: self.nonces,
            authenticator_generations: self.authenticator_generations,
        })
    }
}

impl VerifiedPolicySnapshot {
    pub(crate) fn state(&self) -> &VerifiedPolicySet {
        &self.state
    }
    pub(crate) fn pins(&self) -> &[PolicyPin] {
        &self.pins
    }
    pub(crate) fn heads(&self) -> &[PolicyStamp] {
        &self.heads
    }
    pub(crate) fn journal(&self) -> &[JournalEntry] {
        &self.journal
    }
    pub(crate) fn has_registered_authenticator(
        &self,
        account: &AccountId,
        generation: u64,
    ) -> bool {
        self.authenticator_generations
            .contains(&(account.clone(), generation))
    }
    pub(crate) fn with_independent_pins(&self, incoming: &[PolicyPin]) -> Result<Self> {
        require(
            incoming.len() == self.pins.len()
                && incoming.iter().zip(&self.pins).all(|(pin, saved)| {
                    pin.domain == saved.domain && pin.genesis_root == saved.genesis_root
                }),
            "policy refresh changed independently trusted domains or genesis roots",
        )?;
        let mut pins = self.pins.clone();
        for (pin, saved) in incoming.iter().zip(&mut pins) {
            if let Some(minimum) = &pin.minimum_head {
                require(
                    minimum.domain == pin.domain,
                    "policy minimum belongs to another domain",
                )?;
                if !self.state.roots.contains_key(minimum) {
                    return Err(AuthorizationError::NeedPolicySync(vec![minimum.clone()]));
                }
                let position = minimum_position(&self.state, minimum)?;
                if saved
                    .minimum_head
                    .as_ref()
                    .map(|old| minimum_position(&self.state, old))
                    .transpose()?
                    .is_none_or(|old| old < position)
                {
                    saved.minimum_head = Some(minimum.clone());
                }
            }
        }
        let mut refreshed = self.clone();
        refreshed.pins = pins;
        Ok(refreshed)
    }
    pub(crate) fn into_parts(self) -> (VerifiedPolicySet, Vec<PolicyPin>, Vec<JournalEntry>) {
        (self.state, self.pins, self.journal)
    }

    pub(crate) fn into_import(self) -> BoundedPolicyImport {
        let heads: BTreeSet<_> = self.heads.into_iter().collect();
        BoundedPolicyImport {
            state: self.state,
            pins: self.pins,
            required_heads: heads.clone(),
            seen_heads: heads,
            journal: self.journal,
            entry_index: self.entry_index,
            nonces: self.nonces,
            authenticator_generations: self.authenticator_generations,
        }
    }

    pub(crate) fn apply_delta(&self, entries: &[JournalEntry]) -> Result<Self> {
        require(
            entries.len() <= MAX_PROOF_OBJECTS && canonical(&entries)?.len() <= MAX_PROOF_BYTES,
            "policy delta exceeds the proof bounds",
        )?;
        let mut import = self.clone().into_import();
        for entry in entries {
            import.apply_entry(entry.clone())?;
        }
        let heads = import
            .pins
            .iter()
            .map(|pin| {
                import
                    .state
                    .head(&pin.domain)
                    .cloned()
                    .ok_or(AuthorizationError::PolicyConflict)
            })
            .collect::<Result<Vec<_>>>()?;
        import.finish(&heads)
    }

    pub(crate) fn apply_locally_approved_recovery_delta(
        &self,
        entries: &[JournalEntry],
        approval: &crate::authorization::bootstrap::recovery::stream::approval::LocallyApprovedRecoveryAccount,
    ) -> Result<Self> {
        require(
            !entries.is_empty()
                && entries.len() <= MAX_PROOF_OBJECTS
                && canonical(&entries)?.len() <= MAX_PROOF_BYTES,
            "approved recovery delta exceeds proof bounds",
        )?;
        let first = &entries[0];
        require(
            first == &JournalEntry::Account(approval.account().clone())
                && self.state.head(&approval.expected_head().domain)
                    == Some(approval.expected_head()),
            "approved recovery delta changed its exact account or prefix",
        )?;
        let mut import = self.clone().into_import();
        let hash = entry_hash(first)?;
        require(
            !import.entry_index.contains_key(&hash)
                && import.journal.len() < MAX_POLICY_HISTORY * MAX_POLICY_DOMAINS,
            "approved recovery duplicated accepted history",
        )?;
        allowed(&import.pins, &approval.expected_head().domain)?;
        witness(&import.state, &import.pins, approval.expected_head())?;
        approval.apply(&mut import.state)?;
        account_references(
            &import.state,
            &import.pins,
            &approval.account().payload,
            None,
        )?;
        if let Some(generation) = registered_generation(first) {
            import.authenticator_generations.insert(generation);
        }
        if let Some(head) = import.state.head(&entry_domain(first)) {
            if import.required_heads.contains(head) {
                import.seen_heads.insert(head.clone());
            }
        }
        import.entry_index.insert(hash, import.journal.len());
        import.journal.push(first.clone());
        for entry in &entries[1..] {
            import.apply_entry(entry.clone())?;
        }
        let heads = import
            .pins
            .iter()
            .map(|pin| {
                import
                    .state
                    .head(&pin.domain)
                    .cloned()
                    .ok_or(AuthorizationError::PolicyConflict)
            })
            .collect::<Result<Vec<_>>>()?;
        import.finish(&heads)
    }
}

impl VerifiedPolicySnapshot {
    pub(crate) fn with_created_team(
        &self,
        approved: &crate::authorization::governance::VerifiedTeamCreationDelta,
    ) -> Result<Self> {
        let request: &crate::authorization::governance::TeamCreationRequest =
            &approved.request().payload;
        require(
            self.heads.len() == self.pins.len()
                && self.heads.iter().zip(&self.pins).all(|(head, pin)| {
                    head.domain == pin.domain && self.state.head(&head.domain) == Some(head)
                })
                && canonical(approved.request())?.len() <= MAX_PROOF_BYTES,
            "created team changed its closed verified source policy",
        )?;
        let [
            JournalEntry::SpaceGenesis(genesis),
            JournalEntry::Resource(_),
            JournalEntry::Membership(_),
            JournalEntry::Grant(_),
            JournalEntry::Space(final_policy),
        ] = request.journal_delta.as_slice()
        else {
            return Err(AuthorizationError::InvalidProof(
                "created team requires its exact five policy objects",
            ));
        };
        let domain = PolicyDomain::Space(request.team_project.space.clone());
        let final_head = space_stamp(final_policy)?;
        require(
            request.team_pin.domain == domain
                && request.team_pin.minimum_head.as_ref() == Some(&final_head)
                && request.team_project.space == SpaceId::team(&request.team_pin.genesis_root)
                && genesis.payload.space == request.team_project.space
                && genesis.payload.root
                    == (SpaceRoot::Team {
                        genesis_root: request.team_pin.genesis_root.clone(),
                    })
                && request
                    .journal_delta
                    .iter()
                    .all(|entry| entry_domain(entry) == domain),
            "created team changed its independent genesis or exact delta",
        )?;
        if let Some(existing) = self.pins.iter().find(|pin| pin.domain == domain) {
            let source_heads = approved.source_heads();
            require(
                !source_heads.is_empty()
                    && source_heads.len() <= self.pins.len()
                    && source_heads
                        .windows(2)
                        .all(|pair| pair[0].domain < pair[1].domain)
                    && source_heads.iter().all(|head| {
                        self.pins.iter().any(|pin| pin.domain == head.domain)
                            && self.state.roots.contains_key(head)
                    })
                    && self
                        .pins
                        .iter()
                        .filter(|pin| pin.domain != domain)
                        .all(|pin| source_heads.iter().any(|head| head.domain == pin.domain)),
                "existing team lost an exact approved source dependency",
            )?;
            require(
                existing.genesis_root == request.team_pin.genesis_root
                    && self
                        .state
                        .space_history(&request.team_project.space)
                        .and_then(|history| history.first())
                        == Some(genesis)
                    && self.state.roots.contains_key(&final_head),
                "existing team differs from the approved creation",
            )?;
            for entry in &request.journal_delta {
                let hash = entry_hash(entry)?;
                require(
                    self.entry_index
                        .get(&hash)
                        .and_then(|index| self.journal.get(*index))
                        == Some(entry),
                    "existing team is missing an exact approved creation object",
                )?;
            }
            return Ok(self.clone());
        }
        require(
            self.heads == approved.source_heads()
                && self.pins.len() < MAX_POLICY_DOMAINS
                && self.state.space(&request.team_project.space).is_none(),
            "created team is not a new bounded dependency domain",
        )?;
        let mut import = self.clone().into_import();
        import.pins.push(request.team_pin.clone());
        import.pins.sort_by(|a, b| a.domain.cmp(&b.domain));
        import.required_heads.insert(final_head);
        for entry in &request.journal_delta {
            import.apply_entry(entry.clone())?;
        }
        let heads = import
            .pins
            .iter()
            .map(|pin| {
                import
                    .state
                    .head(&pin.domain)
                    .cloned()
                    .ok_or(AuthorizationError::PolicyConflict)
            })
            .collect::<Result<Vec<_>>>()?;
        import.finish(&heads)
    }

    pub(crate) fn before_team_creation(
        &self,
        approved: &crate::authorization::governance::VerifiedPendingTeamCreation,
    ) -> Result<VerifiedPolicySet> {
        let request: &crate::authorization::governance::TeamCreationRequest =
            &approved.request().payload;
        let excluded = &request.team_pin.domain;
        let heads: &[PolicyStamp] = approved.source_heads();
        let pins: Vec<_> = self
            .pins
            .iter()
            .filter(|pin| &pin.domain != excluded)
            .cloned()
            .collect();
        require(
            !heads.is_empty()
                && heads.len() == pins.len()
                && heads.len() <= MAX_POLICY_DOMAINS
                && heads.windows(2).all(|pair| pair[0].domain < pair[1].domain)
                && heads.iter().zip(&pins).all(|(head, pin)| {
                    head.domain == pin.domain && self.state.roots.contains_key(head)
                })
                && request
                    .expected_heads
                    .iter()
                    .all(|head| heads.contains(head))
                && request.team_pin.domain
                    == PolicyDomain::Space(request.team_project.space.clone()),
            "team creation history requires every exact retained dependency head",
        )?;
        let targets: BTreeMap<_, _> = heads
            .iter()
            .map(|head| (head.domain.clone(), head.clone()))
            .collect();
        let mut before = self.state.clone();
        before
            .accounts
            .retain(|id, _| targets.contains_key(&PolicyDomain::Account(id.clone())));
        before
            .spaces
            .retain(|id, _| targets.contains_key(&PolicyDomain::Space(id.clone())));
        for head in heads {
            match &head.domain {
                PolicyDomain::Account(id) => {
                    let account = before
                        .accounts
                        .get_mut(id)
                        .ok_or(AuthorizationError::PolicyConflict)?;
                    let position = account
                        .history
                        .iter()
                        .position(|entry| {
                            entry.payload.authority_epoch == head.authority_epoch
                                && entry.payload.sequence == head.sequence
                        })
                        .ok_or(AuthorizationError::PolicyConflict)?;
                    require(
                        account_stamp(&account.history[position])? == *head,
                        "team creation account history has no exact head",
                    )?;
                    account.history.truncate(position + 1);
                    account.stamp = head.clone();
                }
                PolicyDomain::Space(id) => {
                    let space = before
                        .spaces
                        .get_mut(id)
                        .ok_or(AuthorizationError::PolicyConflict)?;
                    let position = space
                        .history
                        .iter()
                        .position(|entry| {
                            entry.payload.authority_epoch == head.authority_epoch
                                && entry.payload.sequence == head.sequence
                        })
                        .ok_or(AuthorizationError::PolicyConflict)?;
                    require(
                        space_stamp(&space.history[position])? == *head,
                        "team creation space history has no exact head",
                    )?;
                    space.history.truncate(position + 1);
                    space.stamp = head.clone();
                }
            }
        }
        before.roots.retain(|stamp, _| {
            targets.get(&stamp.domain).is_some_and(|head| {
                (stamp.authority_epoch, stamp.sequence) <= (head.authority_epoch, head.sequence)
            })
        });
        let mut devices = BTreeSet::new();
        let mut authenticators = BTreeSet::new();
        let mut resources = BTreeSet::new();
        let mut memberships = BTreeSet::new();
        let mut grants = BTreeSet::new();
        for account in before.accounts.values() {
            for entry in &account.history {
                devices.extend(entry.payload.devices.iter().cloned());
                authenticators.extend(entry.payload.authenticators.iter().cloned());
            }
        }
        for space in before.spaces.values() {
            for entry in &space.history {
                devices.extend(entry.payload.devices.iter().cloned());
                resources.extend(entry.payload.resources.iter().cloned());
                memberships.extend(entry.payload.memberships.iter().cloned());
                grants.extend(entry.payload.grants.iter().cloned());
                grants.extend(entry.payload.governance_grant.iter().cloned());
            }
        }
        before.devices.retain(|hash, value| {
            devices.contains(hash) && before.roots.contains_key(&value.payload.issued_under)
        });
        before.authenticators.retain(|hash, value| {
            authenticators.contains(hash) && before.roots.contains_key(&value.payload.issued_under)
        });
        before.resources.retain(|hash, value| {
            resources.contains(hash) && before.roots.contains_key(&value.payload.issued_under)
        });
        before.memberships.retain(|hash, value| {
            memberships.contains(hash) && before.roots.contains_key(&value.payload.issued_under)
        });
        before.grants.retain(|hash, value| {
            grants.contains(hash) && before.roots.contains_key(&value.payload.issued_under)
        });
        before
            .authenticator_states
            .retain(|hash, _| before.authenticators.contains_key(hash));
        let policy_hashes: BTreeSet<_> = before
            .roots
            .keys()
            .map(|stamp| stamp.head.clone())
            .collect();
        before
            .commits
            .retain(|_, commit| policy_hashes.contains(&commit.payload.proposal_hash));
        for account in before.accounts.values() {
            let document = &account
                .history
                .last()
                .ok_or(AuthorizationError::PolicyConflict)?
                .payload;
            account_references(&before, &pins, document, None)?;
            if let Some(proof) = &document.device_enrollment {
                for head in &proof.intent.payload.expected_heads {
                    witness(&before, &pins, head)?;
                }
                assertion_reference(&before, &pins, &proof.assertion)?;
            }
            if let Some(approval) = &document.authenticator_approval {
                for head in &approval.intent.payload.expected_heads {
                    witness(&before, &pins, head)?;
                }
                assertion_reference(&before, &pins, &approval.assertion)?;
            }
        }
        for space in before.spaces.values() {
            let document = &space
                .history
                .last()
                .ok_or(AuthorizationError::PolicyConflict)?
                .payload;
            space_references(&before, &pins, document)?;
            before.ensure_personal_current(document)?;
        }
        Ok(before)
    }
}

fn merge_map<K: Ord + Clone, V: PartialEq + Clone>(
    target: &mut BTreeMap<K, V>,
    source: &BTreeMap<K, V>,
) -> Result<()> {
    for (key, value) in source {
        if let Some(previous) = target.get(key) {
            require(
                previous == value,
                "verified policy snapshots contain conflicting objects",
            )?;
        } else {
            target.insert(key.clone(), value.clone());
        }
    }
    Ok(())
}

fn minimum_position(state: &VerifiedPolicySet, head: &PolicyStamp) -> Result<usize> {
    require(
        state.roots.contains_key(head),
        "snapshot minimum has no exact verified witness",
    )?;
    match &head.domain {
        PolicyDomain::Account(account) => state.account_history(account).and_then(|history| {
            history.iter().position(|entry| {
                entry.payload.authority_epoch == head.authority_epoch
                    && entry.payload.sequence == head.sequence
            })
        }),
        PolicyDomain::Space(space) => state.space_history(space).and_then(|history| {
            history.iter().position(|entry| {
                entry.payload.authority_epoch == head.authority_epoch
                    && entry.payload.sequence == head.sequence
            })
        }),
    }
    .ok_or(AuthorizationError::PolicyConflict)
}

pub(crate) fn merge_same_head_snapshots(
    left: &VerifiedPolicySnapshot,
    right: &VerifiedPolicySnapshot,
) -> Result<VerifiedPolicySnapshot> {
    let mut merged = left.clone();
    for pin in &right.pins {
        if let Some(existing) = merged
            .pins
            .iter_mut()
            .find(|existing| existing.domain == pin.domain)
        {
            require(
                existing.genesis_root == pin.genesis_root,
                "snapshot genesis pins differ",
            )?;
            if left.state.head(&pin.domain) != right.state.head(&pin.domain) {
                return Err(AuthorizationError::NeedPolicySync(
                    right
                        .heads
                        .iter()
                        .filter(|head| head.domain == pin.domain)
                        .cloned()
                        .collect(),
                ));
            }
            match &pin.domain {
                PolicyDomain::Account(account) => require(
                    left.state.account_history(account) == right.state.account_history(account),
                    "snapshot account policy histories differ",
                )?,
                PolicyDomain::Space(space) => require(
                    left.state.space_history(space) == right.state.space_history(space),
                    "snapshot space policy histories differ",
                )?,
            }
            if let Some(minimum) = &pin.minimum_head {
                let position = minimum_position(&right.state, minimum)?;
                let stronger = match &existing.minimum_head {
                    Some(old) => minimum_position(&left.state, old)? < position,
                    None => true,
                };
                if stronger {
                    existing.minimum_head = Some(minimum.clone());
                }
            }
        } else {
            merged.pins.push(pin.clone());
        }
    }
    require(
        merged.pins.len() <= MAX_POLICY_DOMAINS,
        "too many merged policy domains",
    )?;
    merged.pins.sort_by(|a, b| a.domain.cmp(&b.domain));
    for (id, value) in &right.state.accounts {
        merged
            .state
            .accounts
            .entry(id.clone())
            .or_insert_with(|| value.clone());
    }
    for (id, value) in &right.state.spaces {
        merged
            .state
            .spaces
            .entry(id.clone())
            .or_insert_with(|| value.clone());
    }
    merge_map(&mut merged.state.devices, &right.state.devices)?;
    merge_map(&mut merged.state.resources, &right.state.resources)?;
    merge_map(
        &mut merged.state.authenticators,
        &right.state.authenticators,
    )?;
    merge_map(&mut merged.state.memberships, &right.state.memberships)?;
    merge_map(&mut merged.state.grants, &right.state.grants)?;
    merge_map(&mut merged.state.roots, &right.state.roots)?;
    merge_map(&mut merged.state.commits, &right.state.commits)?;
    merge_map(&mut merged.nonces, &right.nonces)?;
    merged
        .authenticator_generations
        .extend(right.authenticator_generations.iter().cloned());
    for (hash, floor) in &right.state.authenticator_states {
        if let Some(previous) = merged.state.authenticator_states.get_mut(hash) {
            if previous.sign_count == floor.sign_count {
                require(
                    previous.backup_state == floor.backup_state,
                    "same-counter authenticator states conflict",
                )?;
            } else if floor.sign_count > previous.sign_count {
                *previous = *floor;
            }
        } else {
            merged
                .state
                .authenticator_states
                .insert(hash.clone(), *floor);
        }
    }
    // Merged audit order is private history, not a new public replay anchor.
    for (hash, index) in &right.entry_index {
        let entry = &right.journal[*index];
        if let Some(previous) = merged.entry_index.get(hash) {
            require(
                merged.journal[*previous] == *entry,
                "same-hash policy history entries conflict",
            )?;
        } else {
            merged
                .entry_index
                .insert(hash.clone(), merged.journal.len());
            merged.journal.push(entry.clone());
        }
    }
    require(
        merged.journal.len() <= MAX_POLICY_HISTORY * MAX_POLICY_DOMAINS
            && merged.state.devices.len() <= 4096
            && merged.state.resources.len() <= 16384
            && merged.state.authenticators.len() <= 4096
            && merged.state.memberships.len() <= 4096
            && merged.state.grants.len() <= 16384,
        "merged policy snapshots exceed existing capacity bounds",
    )?;
    merged.heads = merged
        .pins
        .iter()
        .map(|pin| {
            merged
                .state
                .head(&pin.domain)
                .cloned()
                .ok_or(AuthorizationError::PolicyConflict)
        })
        .collect::<Result<_>>()?;
    Ok(merged)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::authorization::tests::Fixture;

    fn account_history() -> (Fixture, Vec<PolicyPin>, Vec<JournalEntry>, Vec<PolicyStamp>) {
        let fixture = Fixture::new();
        let domain = PolicyDomain::Account(fixture.account.clone());
        let history = fixture.state.account_history(&fixture.account).unwrap();
        let mut entries = vec![JournalEntry::AccountGenesis(history[0].clone())];
        entries.extend(
            fixture
                .state
                .devices
                .values()
                .filter(|value| value.payload.owner == domain)
                .cloned()
                .map(JournalEntry::Device),
        );
        entries.extend(history[1..].iter().cloned().map(JournalEntry::Account));
        let heads = vec![fixture.state.head(&domain).unwrap().clone()];
        let pins = vec![PolicyPin {
            domain,
            genesis_root: PublicKey::of(&fixture.root),
            minimum_head: Some(heads[0].clone()),
        }];
        (fixture, pins, entries, heads)
    }

    fn complete(
        pins: &[PolicyPin],
        entries: &[JournalEntry],
        heads: &[PolicyStamp],
    ) -> VerifiedPolicySnapshot {
        let mut import = BoundedPolicyImport::new(pins.to_vec(), heads.to_vec()).unwrap();
        for entry in entries {
            import.apply_entry(entry.clone()).unwrap();
        }
        import.finish(heads).unwrap()
    }

    #[test]
    fn passes_saved_exact_heads_without_early_minimum_rejection() {
        let (fixture, pins, entries, heads) = account_history();
        let snapshot = complete(&pins, &entries, &heads);
        assert_eq!(
            snapshot.state().account(&fixture.account),
            fixture.state.account(&fixture.account)
        );
        assert_eq!(snapshot.heads(), heads);
        assert_eq!(snapshot.journal(), entries);
        let mut required = heads.clone();
        required[0].head = ObjectHash::body(&"another signed fork").unwrap();
        let mut import = BoundedPolicyImport::new(pins, required).unwrap();
        for entry in entries {
            import.apply_entry(entry).unwrap();
        }
        assert!(import.finish(&heads).is_err());
    }

    #[test]
    fn missing_certificate_and_invalid_signature_do_not_advance_the_import() {
        let (fixture, pins, entries, heads) = account_history();
        let mut import = BoundedPolicyImport::new(pins, heads.clone()).unwrap();
        let missing = entries
            .iter()
            .find(|entry| matches!(entry, JournalEntry::Device(_)))
            .unwrap()
            .clone();
        let transition = entries
            .iter()
            .find(|entry| matches!(entry, JournalEntry::Account(_)))
            .unwrap()
            .clone();
        for entry in &entries {
            if entry == &transition {
                break;
            }
            if entry != &missing {
                import.apply_entry(entry.clone()).unwrap();
            }
        }
        let before = import.clone();
        assert!(import.apply_entry(transition.clone()).is_err());
        assert_eq!(import.journal, before.journal);
        assert_eq!(import.seen_heads, before.seen_heads);
        assert_eq!(
            import.state.account(&fixture.account),
            before.state.account(&fixture.account)
        );
        import.apply_entry(missing).unwrap();
        let mut invalid = transition.clone();
        let JournalEntry::Account(value) = &mut invalid else {
            unreachable!()
        };
        value.signature = Signature::parse(&"00".repeat(64)).unwrap();
        let count = import.journal.len();
        assert!(import.apply_entry(invalid).is_err());
        assert_eq!(import.journal.len(), count);
        import.apply_entry(transition).unwrap();
        import.finish(&heads).unwrap();
    }

    #[test]
    fn unknown_domains_and_duplicate_entries_are_rejected() {
        let (_, pins, entries, heads) = account_history();
        let mut import = BoundedPolicyImport::new(pins, heads).unwrap();
        let mut foreign = entries[0].clone();
        let JournalEntry::AccountGenesis(value) = &mut foreign else {
            unreachable!()
        };
        value.payload.account = AccountId::from_root(&PublicKey::parse(&"ac".repeat(32)).unwrap());
        assert!(import.apply_entry(foreign).is_err());
        assert!(import.journal.is_empty());
        import.apply_entry(entries[0].clone()).unwrap();
        assert!(import.apply_entry(entries[0].clone()).is_err());
        assert_eq!(import.journal.len(), 1);
    }

    #[test]
    fn genuine_snapshots_apply_only_new_entries_and_merge_exact_same_heads() {
        let (fixture, pins, entries, heads) = account_history();
        let snapshot = complete(&pins, &entries, &heads);
        let merged = merge_same_head_snapshots(&snapshot, &snapshot).unwrap();
        assert_eq!(merged.journal(), entries);
        let mut next = snapshot.state().account(&fixture.account).unwrap().clone();
        next.previous = Some(heads[0].clone());
        next.sequence += 1;
        let delta = JournalEntry::Account(Signed::sign(next, &fixture.root).unwrap());
        let updated = snapshot.apply_delta(&[delta]).unwrap();
        assert_eq!(updated.journal().len(), entries.len() + 1);
        assert_eq!(snapshot.heads(), heads);
        assert!(matches!(
            merge_same_head_snapshots(&snapshot, &updated),
            Err(AuthorizationError::NeedPolicySync(_))
        ));
        let mut conflicting = snapshot.clone();
        conflicting
            .state
            .devices
            .values_mut()
            .next()
            .unwrap()
            .payload
            .expires_at_ms += 1;
        assert!(merge_same_head_snapshots(&snapshot, &conflicting).is_err());
    }

    #[test]
    fn independent_minima_only_advance_to_an_exact_verified_historical_head() {
        let (_, pins, entries, heads) = account_history();
        let JournalEntry::AccountGenesis(genesis) = &entries[0] else {
            unreachable!()
        };
        let mut earlier = pins.clone();
        earlier[0].minimum_head = Some(account_stamp(genesis).unwrap());
        let snapshot = complete(&earlier, &entries, &heads);
        let refreshed = snapshot.with_independent_pins(&pins).unwrap();
        assert_eq!(refreshed.pins(), pins);
        assert_eq!(refreshed.heads(), snapshot.heads());
        assert_eq!(refreshed.journal(), snapshot.journal());
        assert_eq!(
            refreshed.with_independent_pins(&earlier).unwrap().pins(),
            pins
        );
        earlier[0].minimum_head = None;
        assert_eq!(
            refreshed.with_independent_pins(&earlier).unwrap().pins(),
            pins
        );
        let mut unverified = pins.clone();
        let head = unverified[0].minimum_head.as_mut().unwrap();
        head.sequence += 1;
        head.head = ObjectHash::body(&"unimported successor").unwrap();
        assert!(matches!(
            refreshed.with_independent_pins(&unverified),
            Err(AuthorizationError::NeedPolicySync(_))
        ));
        assert_eq!(refreshed.pins(), pins);
        let mut changed_genesis = pins;
        changed_genesis[0].genesis_root = PublicKey::parse(&"bc".repeat(32)).unwrap();
        assert!(refreshed.with_independent_pins(&changed_genesis).is_err());
        assert!(refreshed.with_independent_pins(&[]).is_err());
    }
}
