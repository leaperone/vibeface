use anyhow::{Context, Result, ensure};
use rusqlite::{Connection, Transaction, params};
use std::collections::{BTreeMap, BTreeSet};
use vibeface_secure::authorization::{
    AccountId, Committed, JournalEntry, ObjectHash, PolicyCommit, PolicyDomain, PolicyPin,
    PolicyStamp, PublicKey, Signed, SpaceId, VerifiedPolicySet,
};

const MAX_UPDATE_ENTRIES: usize = 64;
const MAX_UPDATE_BYTES: usize = 256 * 1024;

#[derive(Clone, Default)]
pub(crate) struct PolicyState {
    state: VerifiedPolicySet,
    pins: Vec<PolicyPin>,
    accepted: BTreeMap<ObjectHash, AcceptedEntry>,
    order: Vec<ObjectHash>,
}

#[derive(Clone, PartialEq, Eq)]
struct AcceptedEntry {
    entry: JournalEntry,
    at_ms: u64,
}

pub(crate) struct PreparedPolicyUpdate {
    before: PolicyState,
    after: PolicyState,
    additions: Vec<(ObjectHash, AcceptedEntry)>,
}

impl PreparedPolicyUpdate {
    pub(crate) fn verified_after(&self) -> &VerifiedPolicySet {
        &self.after.state
    }
}

fn domain_key(domain: &PolicyDomain) -> (&'static str, &str) {
    match domain {
        PolicyDomain::Account(id) => ("account", id.as_str()),
        PolicyDomain::Space(id) => ("space", id.as_str()),
    }
}

fn domain_from_sql(kind: &str, id: &str) -> Result<PolicyDomain> {
    match kind {
        "account" => Ok(PolicyDomain::Account(AccountId::parse(id)?)),
        "space" => Ok(PolicyDomain::Space(SpaceId::parse(id)?)),
        _ => anyhow::bail!("invalid stored policy domain"),
    }
}

fn entry_hash(entry: &JournalEntry) -> Result<ObjectHash> {
    Ok(match entry {
        JournalEntry::AccountGenesis(value) | JournalEntry::Account(value) => value.hash()?,
        JournalEntry::SpaceGenesis(value) | JournalEntry::Space(value) => value.hash()?,
        JournalEntry::Device(value) => value.hash()?,
        JournalEntry::Authenticator(value) => value.hash()?,
        JournalEntry::Membership(value) => value.hash()?,
        JournalEntry::Grant(value) => value.hash()?,
        JournalEntry::Resource(value) => value.hash()?,
        JournalEntry::AccountCommit(value) => value.proposal.hash()?,
        JournalEntry::SpaceCommit(value) => value.proposal.hash()?,
    })
}

fn commit(entry: &JournalEntry) -> Option<&Signed<PolicyCommit>> {
    match entry {
        JournalEntry::AccountCommit(value) => Some(&value.commit),
        JournalEntry::SpaceCommit(value) => Some(&value.commit),
        _ => None,
    }
}

fn same_committed<T: PartialEq>(a: &Committed<T>, b: &Committed<T>) -> bool {
    a.proposal == b.proposal
        && a.commit == b.commit
        && a.receipts.len() == b.receipts.len()
        && a.receipts
            .iter()
            .all(|receipt| b.receipts.contains(receipt))
}

fn same_entry(a: &JournalEntry, b: &JournalEntry) -> bool {
    match (a, b) {
        (JournalEntry::AccountCommit(a), JournalEntry::AccountCommit(b)) => same_committed(a, b),
        (JournalEntry::SpaceCommit(a), JournalEntry::SpaceCommit(b)) => same_committed(a, b),
        _ => a == b,
    }
}

fn apply_entry(
    state: &mut VerifiedPolicySet,
    pins: &[PolicyPin],
    entry: &JournalEntry,
    at_ms: u64,
    replay: bool,
) -> Result<()> {
    match entry {
        JournalEntry::AccountGenesis(value) => {
            let domain = PolicyDomain::Account(value.payload.account.clone());
            let pin = pins
                .iter()
                .find(|pin| pin.domain == domain)
                .context("account genesis is not pinned")?;
            state.trust_account_genesis(&pin.genesis_root, value.clone())?;
        }
        JournalEntry::SpaceGenesis(value) => {
            let domain = PolicyDomain::Space(value.payload.space.clone());
            let pin = pins
                .iter()
                .find(|pin| pin.domain == domain)
                .context("space genesis is not pinned")?;
            state.trust_space_genesis(&pin.genesis_root, value.clone())?;
        }
        JournalEntry::Account(value) => {
            state.install_account(value.clone())?;
        }
        JournalEntry::Space(value) => {
            state.install_space(value.clone())?;
        }
        JournalEntry::Device(value) => {
            state.install_device(value.clone(), at_ms)?;
        }
        JournalEntry::Authenticator(value) => {
            state.install_authenticator(value.clone(), at_ms)?;
        }
        JournalEntry::Membership(value) => {
            state.install_membership(value.clone(), at_ms)?;
        }
        JournalEntry::Grant(value) => {
            state.install_grant(value.clone(), at_ms)?;
        }
        JournalEntry::Resource(value) => {
            state.install_resource(value.clone())?;
        }
        JournalEntry::AccountCommit(value) => {
            if replay {
                state.replay_account_committed(value.clone())?;
            } else {
                state.install_account_committed(value.clone(), at_ms)?;
            }
        }
        JournalEntry::SpaceCommit(value) => {
            if replay {
                state.replay_space_committed(value.clone())?;
            } else {
                state.install_space_committed(value.clone(), at_ms)?;
            }
        }
    }
    Ok(())
}

fn read_pins(connection: &Connection) -> Result<Vec<PolicyPin>> {
    let mut statement = connection.prepare(
        "SELECT domain_kind,domain_id,genesis_root,minimum_head FROM policy_pins ORDER BY domain_kind,domain_id",
    )?;
    let rows = statement.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, Option<String>>(3)?,
        ))
    })?;
    rows.map(|row| {
        let (kind, id, root, minimum) = row?;
        Ok(PolicyPin {
            domain: domain_from_sql(&kind, &id)?,
            genesis_root: PublicKey::parse(&root)?,
            minimum_head: minimum
                .map(|value| serde_json::from_str(&value))
                .transpose()?,
        })
    })
    .collect()
}

fn read_heads(connection: &Connection) -> Result<BTreeMap<PolicyDomain, PolicyStamp>> {
    let mut statement = connection.prepare(
        "SELECT domain_kind,domain_id,authority_epoch,sequence,head_hash FROM current_policy_heads",
    )?;
    let rows = statement.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, u64>(2)?,
            row.get::<_, u64>(3)?,
            row.get::<_, String>(4)?,
        ))
    })?;
    rows.map(|row| {
        let (kind, id, authority_epoch, sequence, hash) = row?;
        let domain = domain_from_sql(&kind, &id)?;
        Ok((
            domain.clone(),
            PolicyStamp {
                domain,
                authority_epoch,
                sequence,
                head: ObjectHash::parse(&hash)?,
            },
        ))
    })
    .collect()
}

impl PolicyState {
    pub(crate) fn verified(&self) -> &VerifiedPolicySet {
        &self.state
    }

    pub(crate) fn pins(&self) -> &[PolicyPin] {
        &self.pins
    }

    pub(crate) fn load(connection: &Connection) -> Result<Self> {
        let mut result = Self {
            pins: read_pins(connection)?,
            ..Self::default()
        };
        let mut statement = connection.prepare(
            "SELECT entry_hash,entry,accepted_at_ms FROM policy_journal ORDER BY ordinal",
        )?;
        let rows = statement.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, u64>(2)?,
            ))
        })?;
        for row in rows {
            let (hash, encoded, at_ms) = row?;
            let hash = ObjectHash::parse(&hash)?;
            let entry: JournalEntry = serde_json::from_str(&encoded)?;
            ensure!(
                entry_hash(&entry)? == hash,
                "stored policy entry hash mismatch"
            );
            apply_entry(&mut result.state, &result.pins, &entry, at_ms, true)?;
            result.order.push(hash.clone());
            result.accepted.insert(hash, AcceptedEntry { entry, at_ms });
        }
        result.check_minimum_heads()?;
        result.check_heads(connection)?;
        result.check_nonces(connection)?;
        Ok(result)
    }

    fn heads(&self) -> BTreeMap<PolicyDomain, PolicyStamp> {
        self.pins
            .iter()
            .filter_map(|pin| {
                self.state
                    .head(&pin.domain)
                    .cloned()
                    .map(|head| (pin.domain.clone(), head))
            })
            .collect()
    }

    fn check_minimum_heads(&self) -> Result<()> {
        for pin in &self.pins {
            ensure!(
                self.state.head(&pin.domain).is_some(),
                "pinned policy genesis is missing"
            );
            let Some(minimum) = &pin.minimum_head else {
                continue;
            };
            ensure!(
                minimum.domain == pin.domain,
                "policy minimum head domain mismatch"
            );
            let found = match &pin.domain {
                PolicyDomain::Account(id) => {
                    self.state.account_history(id).is_some_and(|history| {
                        history.iter().any(|entry| {
                            entry.payload.authority_epoch == minimum.authority_epoch
                                && entry.payload.sequence == minimum.sequence
                                && entry.hash().ok().as_ref() == Some(&minimum.head)
                        })
                    })
                }
                PolicyDomain::Space(id) => self.state.space_history(id).is_some_and(|history| {
                    history.iter().any(|entry| {
                        entry.payload.authority_epoch == minimum.authority_epoch
                            && entry.payload.sequence == minimum.sequence
                            && entry.hash().ok().as_ref() == Some(&minimum.head)
                    })
                }),
            };
            ensure!(
                found,
                "stored policy is older than or conflicts with the pinned minimum head"
            );
        }
        Ok(())
    }

    fn check_heads(&self, connection: &Connection) -> Result<()> {
        ensure!(
            read_heads(connection)? == self.heads(),
            "stored policy projection does not match the verified journal"
        );
        Ok(())
    }

    fn nonce_bindings(&self) -> Result<BTreeMap<(PolicyDomain, String), ObjectHash>> {
        let mut expected = BTreeMap::new();
        for (hash, record) in &self.accepted {
            if let Some(commit) = commit(&record.entry) {
                let key = (
                    commit.payload.previous.domain.clone(),
                    commit.payload.nonce.as_str().to_owned(),
                );
                ensure!(
                    expected.insert(key, hash.clone()).is_none(),
                    "policy commit nonce was reused"
                );
            }
        }
        Ok(expected)
    }

    fn check_nonces(&self, connection: &Connection) -> Result<()> {
        let mut statement = connection
            .prepare("SELECT domain_kind,domain_id,nonce,entry_hash FROM policy_nonces")?;
        let rows = statement.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
            ))
        })?;
        let actual = rows
            .map(|row| {
                let (kind, id, nonce, hash) = row?;
                Ok((
                    (domain_from_sql(&kind, &id)?, nonce),
                    ObjectHash::parse(&hash)?,
                ))
            })
            .collect::<Result<BTreeMap<_, _>>>()?;
        ensure!(
            actual == self.nonce_bindings()?,
            "stored policy nonce bindings do not match the journal"
        );
        Ok(())
    }

    pub(crate) fn prepare_network_update(
        &self,
        journal: &[JournalEntry],
        now_ms: u64,
    ) -> Result<PreparedPolicyUpdate> {
        self.prepare(&[], journal, now_ms)
    }

    pub(crate) fn historical(
        &self,
        expected: &[PolicyStamp],
        admitted_at_ms: u64,
    ) -> Result<VerifiedPolicySet> {
        ensure!(!expected.is_empty(), "historical policy heads are missing");
        let domains: BTreeSet<_> = expected.iter().map(|head| &head.domain).collect();
        ensure!(
            domains.len() == expected.len(),
            "duplicate historical policy domain"
        );
        let mut state = VerifiedPolicySet::new();
        let mut matching = None;
        for hash in &self.order {
            let accepted = &self.accepted[hash];
            if accepted.at_ms > admitted_at_ms {
                break;
            }
            apply_entry(
                &mut state,
                &self.pins,
                &accepted.entry,
                accepted.at_ms,
                true,
            )?;
            if expected
                .iter()
                .all(|head| state.head(&head.domain) == Some(head))
            {
                matching = Some(state.clone());
            } else if matching.is_some() {
                break;
            }
        }
        matching.context("historical policy heads do not identify an accepted journal prefix")
    }

    pub(crate) fn prepare_bootstrap(
        &self,
        pins: &[PolicyPin],
        journal: &[JournalEntry],
        now_ms: u64,
    ) -> Result<PreparedPolicyUpdate> {
        self.prepare(pins, journal, now_ms)
    }

    fn prepare(
        &self,
        pins: &[PolicyPin],
        journal: &[JournalEntry],
        now_ms: u64,
    ) -> Result<PreparedPolicyUpdate> {
        ensure!(
            journal.len() <= MAX_UPDATE_ENTRIES,
            "too many policy update entries"
        );
        ensure!(
            serde_json::to_vec(&(pins, journal))?.len() <= MAX_UPDATE_BYTES,
            "policy update is too large"
        );
        let mut after = self.clone();
        let mut seen = BTreeSet::new();
        for pin in pins {
            ensure!(seen.insert(&pin.domain), "duplicate policy pin");
            if let Some(existing) = after
                .pins
                .iter()
                .find(|existing| existing.domain == pin.domain)
            {
                ensure!(existing == pin, "existing policy pin cannot be replaced");
            } else {
                after.pins.push(pin.clone());
            }
        }
        after.pins.sort_by(|a, b| a.domain.cmp(&b.domain));
        let mut additions = Vec::new();
        for entry in journal {
            let hash = entry_hash(entry)?;
            if let Some(existing) = after.accepted.get(&hash) {
                ensure!(
                    same_entry(&existing.entry, entry),
                    "policy object was replayed with different proof"
                );
                continue;
            }
            apply_entry(&mut after.state, &after.pins, entry, now_ms, false)?;
            let accepted = AcceptedEntry {
                entry: entry.clone(),
                at_ms: now_ms,
            };
            after.order.push(hash.clone());
            after.accepted.insert(hash.clone(), accepted.clone());
            additions.push((hash, accepted));
        }
        after.check_minimum_heads()?;
        after.nonce_bindings()?;
        Ok(PreparedPolicyUpdate {
            before: self.clone(),
            after,
            additions,
        })
    }
}

fn check_snapshot(connection: &Connection, state: &PolicyState) -> Result<()> {
    state.check_heads(connection)?;
    ensure!(
        read_pins(connection)? == state.pins,
        "trusted policy pins changed concurrently"
    );
    let mut statement = connection
        .prepare("SELECT entry_hash,entry,accepted_at_ms FROM policy_journal ORDER BY ordinal")?;
    let rows = statement.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, u64>(2)?,
        ))
    })?;
    let mut count = 0;
    for row in rows {
        let (hash, entry, at_ms) = row?;
        let hash = ObjectHash::parse(&hash)?;
        ensure!(
            state.order.get(count) == Some(&hash),
            "accepted policy journal changed concurrently"
        );
        let entry = serde_json::from_str(&entry)?;
        ensure!(
            state.accepted.get(&hash) == Some(&AcceptedEntry { entry, at_ms }),
            "accepted policy journal changed concurrently"
        );
        count += 1;
    }
    ensure!(
        count == state.accepted.len(),
        "accepted policy journal changed concurrently"
    );
    state.check_nonces(connection)
}

pub(crate) fn commit_in(
    transaction: &Transaction<'_>,
    update: &PreparedPolicyUpdate,
) -> Result<PolicyState> {
    if check_snapshot(transaction, &update.after).is_ok() {
        return Ok(update.after.clone());
    }
    check_snapshot(transaction, &update.before)?;
    transaction.execute_batch("SAVEPOINT policy_journal_update")?;
    let result = (|| -> Result<()> {
        for (hash, accepted) in &update.additions {
            transaction.execute(
                "INSERT INTO policy_journal(entry_hash,entry,accepted_at_ms) VALUES(?,?,?)",
                params![
                    hash.as_str(),
                    serde_json::to_string(&accepted.entry)?,
                    accepted.at_ms
                ],
            )?;
            if let Some(commit) = commit(&accepted.entry) {
                let (kind, id) = domain_key(&commit.payload.previous.domain);
                transaction.execute(
                    "INSERT INTO policy_nonces(domain_kind,domain_id,nonce,entry_hash) VALUES(?,?,?,?)",
                    params![kind, id, commit.payload.nonce.as_str(), hash.as_str()],
                )?;
            }
        }
        for pin in &update.after.pins {
            let (kind, id) = domain_key(&pin.domain);
            transaction.execute(
                "INSERT INTO policy_pins(domain_kind,domain_id,genesis_root,minimum_head) VALUES(?,?,?,?) ON CONFLICT(domain_kind,domain_id) DO NOTHING",
                params![kind, id, pin.genesis_root.as_str(), pin.minimum_head.as_ref().map(serde_json::to_string).transpose()?],
            )?;
        }
        for (domain, stamp) in update.after.heads() {
            let (kind, id) = domain_key(&domain);
            transaction.execute(
                "INSERT INTO current_policy_heads(domain_kind,domain_id,authority_epoch,sequence,head_hash) VALUES(?,?,?,?,?) ON CONFLICT(domain_kind,domain_id) DO UPDATE SET authority_epoch=excluded.authority_epoch,sequence=excluded.sequence,head_hash=excluded.head_hash",
                params![kind, id, stamp.authority_epoch, stamp.sequence, stamp.head.as_str()],
            )?;
        }
        Ok(())
    })();
    match result {
        Ok(()) => {
            transaction.execute_batch("RELEASE policy_journal_update")?;
            Ok(update.after.clone())
        }
        Err(error) => {
            transaction
                .execute_batch("ROLLBACK TO policy_journal_update; RELEASE policy_journal_update")
                .context("policy update rollback failed")?;
            Err(error)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use vibeface_secure::{
        Identity,
        authorization::{
            AUTH_VERSION, AccountDocument, Actor, AuthorityBinding, DeviceCertificate, DeviceId,
            DeviceUse, FreshnessReceipt, Nonce, RecoveryPolicy, SpaceDocument, SpaceRoot,
        },
    };

    const NOW: u64 = 1_000;

    fn key(byte: u8) -> Identity {
        Identity::from_secret_hex(&hex::encode([byte; 32])).unwrap()
    }

    fn database() -> Connection {
        let connection = Connection::open_in_memory().unwrap();
        connection.execute_batch("PRAGMA foreign_keys=ON").unwrap();
        connection
            .execute_batch(include_str!("../../migrations/202610180001_initial.sql"))
            .unwrap();
        connection
    }

    struct Fixture {
        connection: Connection,
        state: PolicyState,
        root: Identity,
        authority: Identity,
        account: AccountId,
        space: SpaceId,
    }

    impl Fixture {
        fn new() -> Self {
            let root = key(1);
            let authority = key(2);
            let root_key = PublicKey::of(&root);
            let account = AccountId::from_root(&root_key);
            let space = SpaceId::personal(&account);
            let recovery = RecoveryPolicy {
                generation: 0,
                keys: vec![PublicKey::of(&key(3))],
                threshold: 1,
            };
            let account_genesis = Signed::sign(
                AccountDocument {
                    version: AUTH_VERSION,
                    account: account.clone(),
                    genesis_root: root_key.clone(),
                    current_root: root_key.clone(),
                    root_generation: 0,
                    previous: None,
                    authority_epoch: 0,
                    sequence: 0,
                    authority: AuthorityBinding::from_key(PublicKey::of(&authority)),
                    recovery: recovery.clone(),
                    enrollment: vec![],
                    authenticators: vec![],
                    authenticator_approval: None,
                    device_enrollment: None,
                    devices: vec![],
                    revoked_devices: vec![],
                    root_acceptance: None,
                    recovery_approvals: vec![],
                },
                &root,
            )
            .unwrap();
            let space_genesis = Signed::sign(
                SpaceDocument {
                    version: AUTH_VERSION,
                    space: space.clone(),
                    root: SpaceRoot::Personal {
                        owner: account.clone(),
                        genesis_root: root_key.clone(),
                    },
                    current_root: root_key.clone(),
                    root_generation: 0,
                    previous: None,
                    authority_epoch: 0,
                    sequence: 0,
                    authority: AuthorityBinding::from_key(PublicKey::of(&authority)),
                    recovery,
                    devices: vec![],
                    revoked_devices: vec![],
                    memberships: vec![],
                    grants: vec![],
                    resources: vec![],
                    governance_grant: None,
                    root_acceptance: None,
                    recovery_approvals: vec![],
                },
                &root,
            )
            .unwrap();
            let pins = vec![
                PolicyPin {
                    domain: PolicyDomain::Account(account.clone()),
                    genesis_root: root_key.clone(),
                    minimum_head: None,
                },
                PolicyPin {
                    domain: PolicyDomain::Space(space.clone()),
                    genesis_root: root_key,
                    minimum_head: None,
                },
            ];
            let mut connection = database();
            let prepared = PolicyState::default()
                .prepare_bootstrap(
                    &pins,
                    &[
                        JournalEntry::AccountGenesis(account_genesis),
                        JournalEntry::SpaceGenesis(space_genesis),
                    ],
                    NOW,
                )
                .unwrap();
            let transaction = connection.transaction().unwrap();
            let state = commit_in(&transaction, &prepared).unwrap();
            transaction.commit().unwrap();
            Self {
                connection,
                state,
                root,
                authority,
                account,
                space,
            }
        }

        fn space_commit(&self, nonce: u8) -> JournalEntry {
            let previous = self
                .state
                .verified()
                .head(&PolicyDomain::Space(self.space.clone()))
                .unwrap()
                .clone();
            let mut document = self.state.verified().space(&self.space).unwrap().clone();
            document.previous = Some(previous.clone());
            document.sequence += 1;
            let proposal = Signed::sign(document, &self.root).unwrap();
            let dependencies: Vec<_> = self.state.heads().into_values().collect();
            let commit = Signed::sign(
                PolicyCommit {
                    version: AUTH_VERSION,
                    previous,
                    proposal_hash: proposal.hash().unwrap(),
                    accepted_at_ms: NOW,
                    issued_at_ms: NOW,
                    expires_at_ms: NOW + 10_000,
                    nonce: Nonce::from_bytes([nonce; 32]),
                    dependencies: dependencies.clone(),
                },
                &self.authority,
            )
            .unwrap();
            let receipts = dependencies
                .into_iter()
                .map(|current| {
                    Signed::sign(
                        FreshnessReceipt {
                            version: AUTH_VERSION,
                            current,
                            challenge_hash: commit.challenge_hash().unwrap(),
                            issued_at_ms: NOW,
                            expires_at_ms: NOW + 10_000,
                        },
                        &self.authority,
                    )
                    .unwrap()
                })
                .collect();
            JournalEntry::SpaceCommit(Committed {
                proposal,
                commit,
                receipts,
            })
        }

        fn accept(&mut self, entries: &[JournalEntry], now_ms: u64) -> Result<()> {
            let prepared = self.state.prepare_network_update(entries, now_ms)?;
            let transaction = self.connection.transaction()?;
            let state = commit_in(&transaction, &prepared)?;
            transaction.commit()?;
            self.state = state;
            Ok(())
        }

        fn count(&self, table: &str) -> u64 {
            self.connection
                .query_row(&format!("SELECT count(*) FROM {table}"), [], |row| {
                    row.get(0)
                })
                .unwrap()
        }
    }

    #[test]
    fn genesis_requires_local_pin_and_bootstrap_cannot_replace_it() {
        let fixture = Fixture::new();
        let genesis = fixture
            .state
            .accepted
            .values()
            .find(|entry| matches!(entry.entry, JournalEntry::AccountGenesis(_)))
            .unwrap()
            .entry
            .clone();
        assert!(
            PolicyState::default()
                .prepare_network_update(&[genesis.clone()], NOW)
                .is_err()
        );
        let wrong = PolicyPin {
            domain: PolicyDomain::Account(fixture.account.clone()),
            genesis_root: PublicKey::of(&key(7)),
            minimum_head: None,
        };
        assert!(
            PolicyState::default()
                .prepare_bootstrap(&[wrong.clone()], &[genesis], NOW)
                .is_err()
        );
        assert!(fixture.state.prepare_bootstrap(&[wrong], &[], NOW).is_err());
        assert_eq!(fixture.count("policy_journal"), 2);
    }

    #[test]
    fn competing_expected_heads_do_not_publish_a_second_commit() {
        let mut fixture = Fixture::new();
        let first = fixture
            .state
            .prepare_network_update(&[fixture.space_commit(7)], NOW)
            .unwrap();
        let second = fixture
            .state
            .prepare_network_update(&[fixture.space_commit(8)], NOW)
            .unwrap();
        let transaction = fixture.connection.transaction().unwrap();
        let accepted = commit_in(&transaction, &first).unwrap();
        transaction.commit().unwrap();
        let transaction = fixture.connection.transaction().unwrap();
        assert!(commit_in(&transaction, &second).is_err());
        transaction.commit().unwrap();
        assert_eq!(fixture.count("policy_journal"), 3);
        assert_eq!(fixture.count("policy_nonces"), 1);
        assert_eq!(
            PolicyState::load(&fixture.connection).unwrap().heads(),
            accepted.heads()
        );
    }

    #[test]
    fn commit_nonce_cannot_bind_another_proposal_or_commit_body() {
        let mut fixture = Fixture::new();
        let first = fixture.space_commit(7);
        fixture.accept(&[first.clone()], NOW).unwrap();
        assert!(
            fixture
                .state
                .prepare_network_update(&[fixture.space_commit(7)], NOW)
                .is_err()
        );
        let mut changed = first;
        if let JournalEntry::SpaceCommit(change) = &mut changed {
            change.commit.payload.expires_at_ms -= 1;
            change.commit =
                Signed::sign(change.commit.payload.clone(), &fixture.authority).unwrap();
        }
        assert!(
            fixture
                .state
                .prepare_network_update(&[changed], NOW)
                .is_err()
        );
        assert_eq!(fixture.count("policy_journal"), 3);
        assert_eq!(fixture.count("policy_nonces"), 1);
    }

    #[test]
    fn failed_projection_write_rolls_back_journal_nonce_and_head() {
        let mut fixture = Fixture::new();
        let initial = fixture.state.heads();
        let update = fixture
            .state
            .prepare_network_update(&[fixture.space_commit(7)], NOW)
            .unwrap();
        fixture.connection.execute_batch(
            "CREATE TRIGGER reject_head BEFORE UPDATE ON current_policy_heads BEGIN SELECT RAISE(ABORT,'injected projection failure'); END",
        ).unwrap();
        let transaction = fixture.connection.transaction().unwrap();
        assert!(commit_in(&transaction, &update).is_err());
        transaction.commit().unwrap();
        assert_eq!(fixture.count("policy_journal"), 2);
        assert_eq!(fixture.count("policy_nonces"), 0);
        assert_eq!(
            PolicyState::load(&fixture.connection).unwrap().heads(),
            initial
        );
    }

    #[test]
    fn duplicate_commit_is_idempotent_after_expiry_and_receipt_reordering() {
        let mut fixture = Fixture::new();
        let entry = fixture.space_commit(7);
        let prepared = fixture
            .state
            .prepare_network_update(&[entry.clone()], NOW)
            .unwrap();
        let transaction = fixture.connection.transaction().unwrap();
        let first = commit_in(&transaction, &prepared).unwrap();
        transaction.commit().unwrap();
        let transaction = fixture.connection.transaction().unwrap();
        assert_eq!(
            commit_in(&transaction, &prepared).unwrap().heads(),
            first.heads()
        );
        transaction.commit().unwrap();
        fixture.state = PolicyState::load(&fixture.connection).unwrap();
        let mut reordered = entry.clone();
        if let JournalEntry::SpaceCommit(value) = &mut reordered {
            value.receipts.reverse();
        }
        fixture.accept(&[reordered], NOW + 100_000).unwrap();
        assert_eq!(fixture.count("policy_journal"), 3);
        assert_eq!(fixture.count("policy_nonces"), 1);
        let mut forged = entry;
        if let JournalEntry::SpaceCommit(value) = &mut forged {
            value.proposal.signature = Signed::sign(value.proposal.payload.clone(), &key(8))
                .unwrap()
                .signature;
        }
        assert!(
            fixture
                .state
                .prepare_network_update(&[forged], NOW)
                .is_err()
        );
    }

    #[test]
    fn load_uses_original_acceptance_time_for_expired_certificates() {
        let mut fixture = Fixture::new();
        let device = key(4);
        let mut certificate = DeviceCertificate {
            version: AUTH_VERSION,
            owner: PolicyDomain::Account(fixture.account.clone()),
            subject: Actor::Account(fixture.account.clone()),
            device: DeviceId::from_key(&PublicKey::of(&device)),
            public_key: PublicKey::of(&device),
            uses: vec![DeviceUse::Noise, DeviceUse::AccountInvocation],
            possession: None,
            enrollment_grant: None,
            issued_under: fixture
                .state
                .verified()
                .head(&PolicyDomain::Account(fixture.account.clone()))
                .unwrap()
                .clone(),
            nonce: Nonce::from_bytes([5; 32]),
            not_before_ms: NOW - 1,
            expires_at_ms: NOW + 1,
        };
        certificate.prove_possession(&device).unwrap();
        let signed = Signed::sign(certificate, &fixture.root).unwrap();
        let mut account = fixture
            .state
            .verified()
            .account(&fixture.account)
            .unwrap()
            .clone();
        account.previous = fixture
            .state
            .verified()
            .head(&PolicyDomain::Account(fixture.account.clone()))
            .cloned();
        account.sequence += 1;
        account.devices.push(signed.hash().unwrap());
        fixture
            .accept(
                &[JournalEntry::Account(
                    Signed::sign(account, &fixture.root).unwrap(),
                )],
                NOW,
            )
            .unwrap();
        let heads: Vec<_> = fixture.state.heads().into_values().collect();
        fixture
            .accept(&[JournalEntry::Device(signed.clone())], NOW)
            .unwrap();
        assert_eq!(
            fixture
                .state
                .historical(&heads, NOW)
                .unwrap()
                .device(&signed.hash().unwrap()),
            Some(&signed)
        );
        let replayed = PolicyState::load(&fixture.connection).unwrap();
        assert_eq!(
            replayed.verified().device(&signed.hash().unwrap()),
            Some(&signed)
        );
        assert_eq!(
            replayed
                .state
                .clone()
                .install_device(signed, NOW + 100_000)
                .unwrap_err(),
            vibeface_secure::authorization::AuthorizationError::Expired
        );
    }

    #[test]
    fn load_rejects_extra_heads_and_a_minimum_outside_verified_history() {
        let fixture = Fixture::new();
        let pin = fixture.state.pins()[0].clone();
        let head = fixture.state.verified().head(&pin.domain).unwrap();
        let mut minimum = head.clone();
        minimum.authority_epoch += 1;
        let (kind, id) = domain_key(&pin.domain);
        fixture
            .connection
            .execute(
                "UPDATE policy_pins SET minimum_head=? WHERE domain_kind=? AND domain_id=?",
                params![serde_json::to_string(&minimum).unwrap(), kind, id],
            )
            .unwrap();
        assert!(PolicyState::load(&fixture.connection).is_err());
        fixture
            .connection
            .execute("UPDATE policy_pins SET minimum_head=NULL", [])
            .unwrap();
        fixture.connection.execute(
            "INSERT INTO current_policy_heads(domain_kind,domain_id,authority_epoch,sequence,head_hash) VALUES('account',?,0,0,?)",
            params![AccountId::from_root(&PublicKey::of(&key(9))).as_str(), head.head.as_str()],
        ).unwrap();
        assert!(PolicyState::load(&fixture.connection).is_err());
    }

    #[test]
    fn historical_reconstruction_distinguishes_heads_at_the_same_millisecond() {
        let mut fixture = Fixture::new();
        let before: Vec<_> = fixture.state.heads().into_values().collect();
        fixture.accept(&[fixture.space_commit(7)], NOW).unwrap();
        let first: Vec<_> = fixture.state.heads().into_values().collect();
        fixture.accept(&[fixture.space_commit(8)], NOW).unwrap();
        for expected in [&before, &first] {
            let historical = fixture.state.historical(expected, NOW).unwrap();
            for head in expected {
                assert_eq!(historical.head(&head.domain), Some(head));
            }
        }
        assert!(fixture.state.historical(&first, NOW - 1).is_err());
        assert!(
            fixture
                .state
                .historical(&[first[0].clone(), first[0].clone()], NOW)
                .is_err()
        );
    }

    #[test]
    fn update_bounds_reject_oversized_batches_before_any_write() {
        let fixture = Fixture::new();
        let entry = fixture.space_commit(7);
        assert!(
            fixture
                .state
                .prepare_network_update(&vec![entry; MAX_UPDATE_ENTRIES + 1], NOW)
                .is_err()
        );
        let mut oversized = fixture.space_commit(7);
        if let JournalEntry::SpaceCommit(value) = &mut oversized {
            value.receipts = vec![value.receipts[0].clone(); 1024];
        }
        assert_eq!(
            fixture
                .state
                .prepare_network_update(&[oversized], NOW)
                .err()
                .unwrap()
                .to_string(),
            "policy update is too large"
        );
        assert_eq!(fixture.count("policy_journal"), 2);
    }
}
