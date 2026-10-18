use anyhow::{Context, Result, ensure};
use rusqlite::{Connection, OptionalExtension, Transaction, params};
use std::collections::{BTreeMap, BTreeSet};
use vibeface_secure::authorization::{
    AccountId, Committed, JournalEntry, ObjectHash, PolicyCommit, PolicyDomain, PolicyPin,
    PolicyStamp, PublicKey, Signed, SpaceId, VerifiedPolicySet,
};

const MAX_UPDATE_ENTRIES: usize = 64;
const MAX_UPDATE_BYTES: usize = 256 * 1024;
const MAX_RECOVERY_SCAN_ENTRIES: usize = 32;

#[derive(Clone, Default)]
pub(crate) struct PolicyState {
    state: VerifiedPolicySet,
    pins: Vec<PolicyPin>,
    accepted: BTreeMap<ObjectHash, AcceptedEntry>,
    order: Vec<ObjectHash>,
    pub(crate) local_admissions: super::admission::AdmissionCheckpoints,
    pub(crate) task_execution_policy:
        super::foreign_execution_policy::TaskExecutionPolicyProvenance,
    pub(crate) gateway_policy: super::gateway_forward::GatewayPolicyCache,
}

#[derive(Clone, PartialEq, Eq)]
struct AcceptedEntry {
    entry: JournalEntry,
    encoded: String,
    received_at_ms: u64,
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct AcceptedJournalSnapshot {
    accepted_len: usize,
    last_hash: Option<ObjectHash>,
    last_ordinal: Option<i64>,
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct AcceptedJournalCursor {
    next_index: usize,
    after_ordinal: Option<i64>,
}

#[derive(Clone, Debug)]
pub(crate) struct AcceptedRecoveryEntry {
    pub(crate) entry_hash: ObjectHash,
    pub(crate) entry: JournalEntry,
    pub(crate) accepted_at_ms: u64,
}

#[derive(Clone, Debug)]
pub(crate) struct AcceptedJournalPage {
    pub(crate) entry: Option<AcceptedRecoveryEntry>,
    pub(crate) next_cursor: AcceptedJournalCursor,
    pub(crate) scanned: usize,
    pub(crate) complete: bool,
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

fn replay_at_ms(entry: &JournalEntry) -> Option<u64> {
    match entry {
        JournalEntry::Device(value) => Some(value.payload.not_before_ms),
        JournalEntry::Authenticator(value) => Some(value.payload.not_before_ms),
        JournalEntry::Membership(value) => Some(value.payload.not_before_ms),
        JournalEntry::Grant(value) => Some(value.payload.not_before_ms),
        JournalEntry::AccountCommit(value) => Some(value.commit.payload.accepted_at_ms),
        JournalEntry::SpaceCommit(value) => Some(value.commit.payload.accepted_at_ms),
        _ => None,
    }
}

fn issued_under(entry: &JournalEntry) -> Option<&PolicyStamp> {
    match entry {
        JournalEntry::Device(value) => Some(&value.payload.issued_under),
        JournalEntry::Authenticator(value) => Some(&value.payload.issued_under),
        JournalEntry::Membership(value) => Some(&value.payload.issued_under),
        JournalEntry::Grant(value) => Some(&value.payload.issued_under),
        JournalEntry::Resource(value) => Some(&value.payload.issued_under),
        _ => None,
    }
}

fn entry_matches_domains(entry: &JournalEntry, domains: &BTreeSet<PolicyDomain>) -> bool {
    let domain = match entry {
        JournalEntry::AccountGenesis(value) | JournalEntry::Account(value) => {
            PolicyDomain::Account(value.payload.account.clone())
        }
        JournalEntry::SpaceGenesis(value) | JournalEntry::Space(value) => {
            PolicyDomain::Space(value.payload.space.clone())
        }
        JournalEntry::AccountCommit(value) => {
            PolicyDomain::Account(value.proposal.payload.account.clone())
        }
        JournalEntry::SpaceCommit(value) => {
            PolicyDomain::Space(value.proposal.payload.space.clone())
        }
        _ => issued_under(entry)
            .expect("policy object has an issued-under domain")
            .domain
            .clone(),
    };
    domains.contains(&domain)
}

fn checked_accepted_ordinal(
    connection: &Connection,
    hash: &ObjectHash,
    accepted: &AcceptedEntry,
) -> Result<i64> {
    let (ordinal, bytes, clock): (i64, u64, u64) = connection
        .query_row(
            "SELECT ordinal,length(CAST(entry AS BLOB)),accepted_at_ms FROM policy_journal WHERE entry_hash=?",
            [hash.as_str()],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .context("accepted recovery journal boundary is missing")?;
    ensure!(
        bytes == accepted.encoded.len() as u64 && clock == accepted.received_at_ms,
        "accepted recovery journal boundary changed"
    );
    let encoded: String = connection.query_row(
        "SELECT entry FROM policy_journal WHERE entry_hash=? AND ordinal=?",
        params![hash.as_str(), ordinal],
        |row| row.get(0),
    )?;
    ensure!(
        encoded == accepted.encoded,
        "accepted recovery journal boundary changed"
    );
    Ok(ordinal)
}

fn apply_entry(
    state: &mut VerifiedPolicySet,
    pins: &[PolicyPin],
    entry: &JournalEntry,
    at_ms: u64,
    replay: bool,
) -> Result<()> {
    let at_ms = if replay {
        replay_at_ms(entry).unwrap_or(at_ms)
    } else {
        at_ms
    };
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

    pub(crate) fn accepted_journal_snapshot(
        &self,
        connection: &Connection,
    ) -> Result<AcceptedJournalSnapshot> {
        let last_hash = self.order.last().cloned();
        let last_ordinal = if let Some(hash) = &last_hash {
            Some(checked_accepted_ordinal(
                connection,
                hash,
                &self.accepted[hash],
            )?)
        } else {
            let nonempty: bool =
                connection.query_row("SELECT EXISTS(SELECT 1 FROM policy_journal)", [], |row| {
                    row.get(0)
                })?;
            ensure!(!nonempty, "unverified recovery journal entries exist");
            None
        };
        let durable_tail: Option<(String, i64)> = connection
            .query_row(
                "SELECT entry_hash,ordinal FROM policy_journal ORDER BY ordinal DESC LIMIT 1",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        ensure!(
            durable_tail
                .as_ref()
                .map(|(hash, ordinal)| (hash.as_str(), *ordinal))
                == last_hash
                    .as_ref()
                    .zip(last_ordinal)
                    .map(|(hash, ordinal)| (hash.as_str(), ordinal)),
            "unverified policy journal tail changed the current frontier"
        );
        Ok(AcceptedJournalSnapshot {
            accepted_len: self.order.len(),
            last_hash,
            last_ordinal,
        })
    }

    pub(crate) fn accepted_journal_prefix(
        &self,
        snapshot: &AcceptedJournalSnapshot,
    ) -> Result<Vec<JournalEntry>> {
        ensure!(
            snapshot.accepted_len <= self.order.len()
                && snapshot.last_hash.as_ref()
                    == snapshot
                        .accepted_len
                        .checked_sub(1)
                        .and_then(|index| self.order.get(index))
                && snapshot.last_ordinal.is_some() == (snapshot.accepted_len != 0),
            "accepted policy prefix differs from its completed read"
        );
        self.order[..snapshot.accepted_len]
            .iter()
            .map(|hash| {
                self.accepted
                    .get(hash)
                    .map(|entry| entry.entry.clone())
                    .context("accepted policy prefix has a missing entry")
            })
            .collect()
    }

    // The caller keeps this read and its recovery cursor update in one transaction.
    pub(crate) fn accepted_journal_page(
        &self,
        connection: &Connection,
        snapshot: &AcceptedJournalSnapshot,
        cursor: Option<&AcceptedJournalCursor>,
        domains: &BTreeSet<PolicyDomain>,
    ) -> Result<AcceptedJournalPage> {
        ensure!(
            snapshot.accepted_len <= self.order.len()
                && snapshot.last_hash.as_ref()
                    == snapshot
                        .accepted_len
                        .checked_sub(1)
                        .and_then(|index| self.order.get(index))
                && snapshot.last_ordinal.is_some() == (snapshot.accepted_len != 0),
            "invalid accepted recovery journal snapshot"
        );
        if let Some(hash) = &snapshot.last_hash {
            ensure!(
                Some(checked_accepted_ordinal(
                    connection,
                    hash,
                    &self.accepted[hash]
                )?) == snapshot.last_ordinal,
                "accepted recovery journal snapshot boundary moved"
            );
        }
        let mut next_cursor = cursor.cloned().unwrap_or(AcceptedJournalCursor {
            next_index: 0,
            after_ordinal: None,
        });
        ensure!(
            next_cursor.next_index <= snapshot.accepted_len
                && next_cursor.after_ordinal.is_some() == (next_cursor.next_index != 0),
            "invalid accepted recovery journal cursor"
        );
        if let Some(index) = next_cursor.next_index.checked_sub(1) {
            let hash = &self.order[index];
            ensure!(
                Some(checked_accepted_ordinal(
                    connection,
                    hash,
                    &self.accepted[hash]
                )?) == next_cursor.after_ordinal,
                "accepted recovery journal cursor boundary moved"
            );
        }
        if next_cursor.next_index == snapshot.accepted_len {
            ensure!(
                next_cursor.after_ordinal == snapshot.last_ordinal,
                "accepted recovery journal did not reach its snapshot boundary"
            );
            return Ok(AcceptedJournalPage {
                entry: None,
                next_cursor,
                scanned: 0,
                complete: true,
            });
        }
        let start = match next_cursor.after_ordinal {
            Some(ordinal) => ordinal
                .checked_add(1)
                .context("accepted recovery journal cursor overflow")?,
            None => i64::MIN,
        };
        let limit = MAX_RECOVERY_SCAN_ENTRIES.min(snapshot.accepted_len - next_cursor.next_index);
        let mut statement = connection.prepare(
            "SELECT ordinal,entry_hash,length(CAST(entry AS BLOB)),accepted_at_ms FROM policy_journal WHERE ordinal>=? AND ordinal<=? ORDER BY ordinal LIMIT ?",
        )?;
        let rows = statement
            .query_map(params![start, snapshot.last_ordinal, limit as u64], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, u64>(2)?,
                    row.get::<_, u64>(3)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        ensure!(
            !rows.is_empty(),
            "accepted recovery journal prefix was deleted"
        );
        let mut entry = None;
        let mut scanned = 0;
        for (ordinal, hash, bytes, clock) in rows {
            let expected = &self.order[next_cursor.next_index];
            let accepted = &self.accepted[expected];
            ensure!(
                expected.as_str() == hash
                    && accepted.encoded.len() as u64 == bytes
                    && accepted.received_at_ms == clock,
                "accepted recovery journal prefix changed or reordered"
            );
            ensure!(
                checked_accepted_ordinal(connection, expected, accepted)? == ordinal,
                "accepted recovery journal prefix moved"
            );
            next_cursor.next_index += 1;
            next_cursor.after_ordinal = Some(ordinal);
            scanned += 1;
            if entry_matches_domains(&accepted.entry, domains) {
                entry = Some(AcceptedRecoveryEntry {
                    entry_hash: expected.clone(),
                    entry: accepted.entry.clone(),
                    accepted_at_ms: accepted.received_at_ms,
                });
                break;
            }
        }
        let complete = next_cursor.next_index == snapshot.accepted_len;
        ensure!(
            !complete || next_cursor.after_ordinal == snapshot.last_ordinal,
            "accepted recovery journal has extra prefix entries"
        );
        ensure!(
            complete || entry.is_some() || scanned == limit,
            "accepted recovery journal prefix was truncated"
        );
        Ok(AcceptedJournalPage {
            entry,
            next_cursor,
            scanned,
            complete,
        })
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
            let (hash, encoded, received_at_ms) = row?;
            let hash = ObjectHash::parse(&hash)?;
            let entry: JournalEntry = serde_json::from_str(&encoded)?;
            ensure!(
                entry_hash(&entry)? == hash,
                "stored policy entry hash mismatch"
            );
            apply_entry(
                &mut result.state,
                &result.pins,
                &entry,
                received_at_ms,
                true,
            )?;
            result.order.push(hash.clone());
            result.accepted.insert(
                hash,
                AcceptedEntry {
                    entry,
                    encoded,
                    received_at_ms,
                },
            );
        }
        result.check_minimum_heads()?;
        result.check_heads(connection)?;
        result.check_nonces(connection)?;
        Ok(result)
    }

    pub(crate) fn reload_verified(connection: &Connection, known: &Self) -> Result<Self> {
        let pins = read_pins(connection)?;
        for pin in &known.pins {
            ensure!(
                pins.iter().find(|current| current.domain == pin.domain) == Some(pin),
                "trusted policy pin changed or was removed"
            );
        }
        let mut result = known.clone();
        result.pins = pins;
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
        let mut count = 0;
        for row in rows {
            let (hash, encoded, received_at_ms) = row?;
            let hash = ObjectHash::parse(&hash)?;
            if count < known.order.len() {
                ensure!(
                    known.order.get(count) == Some(&hash)
                        && known.accepted.get(&hash).is_some_and(|accepted| {
                            accepted.encoded == encoded && accepted.received_at_ms == received_at_ms
                        }),
                    "verified policy journal prefix was replaced or reordered"
                );
                count += 1;
                continue;
            }
            let entry: JournalEntry = serde_json::from_str(&encoded)?;
            ensure!(
                entry_hash(&entry)? == hash,
                "stored policy entry hash mismatch"
            );
            let accepted = AcceptedEntry {
                entry,
                encoded,
                received_at_ms,
            };
            ensure!(
                !result.accepted.contains_key(&hash),
                "duplicate policy journal entry"
            );
            apply_entry(
                &mut result.state,
                &result.pins,
                &accepted.entry,
                received_at_ms,
                true,
            )?;
            result.order.push(hash.clone());
            result.accepted.insert(hash, accepted);
            count += 1;
        }
        ensure!(
            count >= known.order.len(),
            "verified policy journal prefix was removed"
        );
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
        self.prepare(&[], journal, now_ms, false)
    }

    pub(crate) fn current_heads_at(
        &self,
        expected: &[PolicyStamp],
        admitted_at_ms: u64,
    ) -> Result<bool> {
        if expected
            .iter()
            .any(|head| self.state.head(&head.domain) != Some(head))
        {
            return Ok(false);
        }
        self.historical_dependencies(expected, admitted_at_ms)?;
        Ok(true)
    }

    fn historical_dependencies(
        &self,
        expected: &[PolicyStamp],
        admitted_at_ms: u64,
    ) -> Result<(BTreeSet<ObjectHash>, Vec<ObjectHash>)> {
        ensure!(!expected.is_empty(), "historical policy heads are missing");
        let domains: BTreeSet<_> = expected.iter().map(|head| &head.domain).collect();
        ensure!(
            domains.len() == expected.len(),
            "duplicate historical policy domain"
        );
        let mut policies: Vec<_> = expected.iter().map(|head| head.head.clone()).collect();
        let mut policy_hashes = BTreeSet::new();
        let mut objects = Vec::new();
        while let Some(hash) = policies.pop() {
            if !policy_hashes.insert(hash.clone()) {
                continue;
            }
            let entry = &self
                .accepted
                .get(&hash)
                .context("historical policy head is missing")?
                .entry;
            let account = match entry {
                JournalEntry::AccountGenesis(value) | JournalEntry::Account(value) => Some(value),
                JournalEntry::AccountCommit(value) => Some(&value.proposal),
                _ => None,
            };
            let space = match entry {
                JournalEntry::SpaceGenesis(value) | JournalEntry::Space(value) => Some(value),
                JournalEntry::SpaceCommit(value) => Some(&value.proposal),
                _ => None,
            };
            ensure!(
                account.is_some() || space.is_some(),
                "historical head is not a policy"
            );
            if let Some(value) = account {
                policies.extend(value.payload.previous.iter().map(|head| head.head.clone()));
                objects.extend(value.payload.devices.iter().cloned());
                objects.extend(value.payload.authenticators.iter().cloned());
                if let Some(proof) = &value.payload.device_enrollment {
                    policies.extend(
                        proof
                            .intent
                            .payload
                            .expected_heads
                            .iter()
                            .map(|head| head.head.clone()),
                    );
                }
                if let Some(proof) = &value.payload.authenticator_approval {
                    policies.extend(
                        proof
                            .intent
                            .payload
                            .expected_heads
                            .iter()
                            .map(|head| head.head.clone()),
                    );
                }
            }
            if let Some(value) = space {
                policies.extend(value.payload.previous.iter().map(|head| head.head.clone()));
                objects.extend(value.payload.devices.iter().cloned());
                objects.extend(value.payload.memberships.iter().cloned());
                objects.extend(value.payload.grants.iter().cloned());
                objects.extend(value.payload.resources.iter().cloned());
                objects.extend(value.payload.governance_grant.iter().cloned());
            }
            if let Some(commit) = commit(entry) {
                ensure!(
                    commit.payload.accepted_at_ms <= admitted_at_ms,
                    "historical policy commit is in the future"
                );
                policies.extend(
                    commit
                        .payload
                        .dependencies
                        .iter()
                        .map(|head| head.head.clone()),
                );
            }
        }
        Ok((policy_hashes, objects))
    }

    pub(crate) fn historical(
        &self,
        expected: &[PolicyStamp],
        admitted_at_ms: u64,
    ) -> Result<VerifiedPolicySet> {
        let (policy_hashes, mut objects) =
            self.historical_dependencies(expected, admitted_at_ms)?;
        let mut selected = policy_hashes.clone();
        while let Some(hash) = objects.pop() {
            if selected.contains(&hash) {
                continue;
            }
            let Some(accepted) = self.accepted.get(&hash) else {
                continue;
            };
            let Some(stamp) = issued_under(&accepted.entry) else {
                continue;
            };
            if !policy_hashes.contains(&stamp.head)
                || replay_at_ms(&accepted.entry).is_some_and(|at| at > admitted_at_ms)
            {
                continue;
            }
            selected.insert(hash);
            match &accepted.entry {
                JournalEntry::Device(value) => {
                    objects.extend(value.payload.enrollment_grant.iter().cloned())
                }
                JournalEntry::Grant(value) => objects.extend(value.payload.parent.iter().cloned()),
                _ => {}
            }
        }
        let mut state = VerifiedPolicySet::new();
        for hash in self.order.iter().filter(|hash| selected.contains(*hash)) {
            apply_entry(
                &mut state,
                &self.pins,
                &self.accepted[hash].entry,
                admitted_at_ms,
                true,
            )?;
        }
        ensure!(
            expected
                .iter()
                .all(|head| state.head(&head.domain) == Some(head)),
            "historical policy heads do not identify the verified history"
        );
        Ok(state)
    }

    pub(crate) fn prepare_bootstrap(
        &self,
        pins: &[PolicyPin],
        journal: &[JournalEntry],
        now_ms: u64,
    ) -> Result<PreparedPolicyUpdate> {
        self.prepare(pins, journal, now_ms, true)
    }

    pub(crate) fn prepare_account_recovery_update(
        &self,
        verified: vibeface_secure::authorization::bootstrap::recovery::stream::VerifiedStreamedRecoveryCommit,
        now_ms: u64,
    ) -> Result<PreparedPolicyUpdate> {
        let expected = verified.expected_heads().to_vec();
        let (state, pins, delta) = verified.into_store_parts();
        self.prepare_verified_recovery_delta(state, &pins, &delta, &expected, now_ms)
    }

    pub(crate) fn prepare_cross_recovery_commit(
        &self,
        verified: vibeface_secure::authorization::bootstrap::recovery::stream::VerifiedStreamedCrossNodeRecoveryCommit,
        now_ms: u64,
    ) -> Result<PreparedPolicyUpdate> {
        let expected = verified.expected_heads().to_vec();
        let (state, pins, delta) = verified.into_store_parts();
        self.prepare_verified_recovery_delta(state, &pins, &delta, &expected, now_ms)
    }

    fn prepare_verified_recovery_delta(
        &self,
        state: VerifiedPolicySet,
        pins: &[PolicyPin],
        delta: &[JournalEntry],
        expected: &[PolicyStamp],
        now_ms: u64,
    ) -> Result<PreparedPolicyUpdate> {
        ensure!(
            !expected.is_empty()
                && expected
                    .iter()
                    .all(|head| self.state.head(&head.domain) == Some(head)),
            "recovery source policy changed before commit"
        );
        ensure!(
            !delta.is_empty()
                && delta.len() <= MAX_UPDATE_ENTRIES
                && serde_json::to_vec(delta)?.len() <= MAX_UPDATE_BYTES,
            "invalid verified recovery delta size"
        );
        for pin in pins {
            ensure!(
                self.pins
                    .iter()
                    .any(|existing| existing.domain == pin.domain
                        && existing.genesis_root == pin.genesis_root),
                "recovery cannot introduce or replace a trusted source pin"
            );
        }
        for pin in &self.pins {
            if expected.iter().all(|head| head.domain != pin.domain) {
                ensure!(
                    state.head(&pin.domain) == self.state.head(&pin.domain),
                    "recovery changed an unrelated source policy domain"
                );
            }
        }
        let mut after = self.clone();
        after.state = state;
        let mut additions = Vec::with_capacity(delta.len());
        for entry in delta {
            let hash = entry_hash(entry)?;
            ensure!(
                !after.accepted.contains_key(&hash),
                "recovery delta repeats accepted history"
            );
            let accepted = AcceptedEntry {
                entry: entry.clone(),
                encoded: serde_json::to_string(entry)?,
                received_at_ms: now_ms,
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

    pub(crate) fn prepare_cross_recovery_install(
        &self,
        verified: vibeface_secure::authorization::bootstrap::recovery::stream::VerifiedCrossNodeRecoveryInstallation,
        now_ms: u64,
    ) -> Result<PreparedPolicyUpdate> {
        let prefix_len = verified.accepted_prefix_len();
        let (state, pins, journal) = verified.into_store_parts();
        self.prepare_verified_initial_installation(state, pins, journal, prefix_len, now_ms)
    }

    pub(crate) fn prepare_account_node_installation(
        &self,
        verified: vibeface_secure::authorization::bootstrap::recovery::node_enrollment::VerifiedAccountNodeInstallation,
        now_ms: u64,
    ) -> Result<PreparedPolicyUpdate> {
        let prefix_len = verified.accepted_prefix_len();
        let (state, pins, journal) = verified.into_store_parts();
        self.prepare_verified_initial_installation(state, pins, journal, prefix_len, now_ms)
    }

    fn prepare_verified_initial_installation(
        &self,
        state: VerifiedPolicySet,
        mut pins: Vec<PolicyPin>,
        journal: Vec<JournalEntry>,
        prefix_len: usize,
        now_ms: u64,
    ) -> Result<PreparedPolicyUpdate> {
        ensure!(
            self.accepted.is_empty() && self.order.is_empty() && self.pins.is_empty(),
            "verified policy installation requires an unclaimed Node"
        );
        ensure!(
            prefix_len != 0 && prefix_len < journal.len(),
            "verified policy installation has no accepted prefix or delta"
        );
        pins.sort_by(|left, right| left.domain.cmp(&right.domain));
        ensure!(
            pins.windows(2).all(|pair| pair[0].domain != pair[1].domain),
            "verified policy installation has duplicate pins"
        );
        let mut after = Self {
            state,
            pins,
            accepted: BTreeMap::new(),
            order: Vec::new(),
            local_admissions: Default::default(),
            task_execution_policy: Default::default(),
            gateway_policy: Default::default(),
        };
        let mut additions = Vec::with_capacity(journal.len());
        for entry in journal {
            let hash = entry_hash(&entry)?;
            let accepted = AcceptedEntry {
                encoded: serde_json::to_string(&entry)?,
                entry,
                received_at_ms: now_ms,
            };
            ensure!(
                !after.accepted.contains_key(&hash),
                "verified policy installation repeats a journal object"
            );
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

    pub(crate) fn prepare_team_creation(
        &self,
        verified: &vibeface_secure::authorization::governance::VerifiedTeamCreationRequest,
        now_ms: u64,
    ) -> Result<PreparedPolicyUpdate> {
        let request = verified.request();
        ensure!(
            request.issued_at_ms <= now_ms && now_ms < verified.expires_at_ms(),
            "team creation request expired"
        );
        ensure!(
            verified
                .expected_heads()
                .iter()
                .all(|head| self.state.head(&head.domain) == Some(head)),
            "team creation source policy changed"
        );
        let pin = &request.team_pin;
        ensure!(
            pin.domain == PolicyDomain::Space(SpaceId::team(&pin.genesis_root))
                && pin.domain == PolicyDomain::Space(request.team_project.space.clone())
                && self.state.head(&pin.domain).is_none()
                && self
                    .pins
                    .iter()
                    .all(|existing| existing.domain != pin.domain),
            "team creation requires a new team policy domain"
        );
        self.prepare(
            std::slice::from_ref(pin),
            &request.journal_delta,
            now_ms,
            false,
        )
    }

    fn prepare(
        &self,
        pins: &[PolicyPin],
        journal: &[JournalEntry],
        now_ms: u64,
        replay: bool,
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
            ensure!(
                !replay || replay_at_ms(entry).is_none_or(|at| at <= now_ms),
                "policy history begins after its receipt"
            );
            apply_entry(&mut after.state, &after.pins, entry, now_ms, replay)?;
            let accepted = AcceptedEntry {
                entry: entry.clone(),
                encoded: serde_json::to_string(entry)?,
                received_at_ms: now_ms,
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
        let (hash, entry, received_at_ms) = row?;
        let hash = ObjectHash::parse(&hash)?;
        ensure!(
            state.order.get(count) == Some(&hash),
            "accepted policy journal changed concurrently"
        );
        let encoded = entry;
        let entry = serde_json::from_str(&encoded)?;
        ensure!(
            state.accepted.get(&hash)
                == Some(&AcceptedEntry {
                    entry,
                    encoded,
                    received_at_ms
                }),
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
                params![hash.as_str(), &accepted.encoded, accepted.received_at_ms],
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

    fn append_fixture(target: &mut Fixture, other: &Fixture) {
        let journal = other
            .state
            .order
            .iter()
            .map(|hash| other.state.accepted[hash].entry.clone())
            .collect::<Vec<_>>();
        let update = target
            .state
            .prepare_bootstrap(&other.state.pins, &journal, NOW)
            .unwrap();
        let tx = target.connection.transaction().unwrap();
        target.state = commit_in(&tx, &update).unwrap();
        tx.commit().unwrap();
    }

    #[test]
    fn accepted_pages_bound_scanning_and_preserve_snapshot_and_exact_retries() {
        let mut fixture = Fixture::with_root(40);
        for seed in 41..56 {
            append_fixture(&mut fixture, &Fixture::with_root(seed));
        }
        let selected = Fixture::new();
        append_fixture(&mut fixture, &selected);
        let domains = BTreeSet::from([
            PolicyDomain::Account(selected.account.clone()),
            PolicyDomain::Space(selected.space.clone()),
        ]);
        let snapshot = fixture
            .state
            .accepted_journal_snapshot(&fixture.connection)
            .unwrap();
        assert_eq!(snapshot.accepted_len, 34);
        let stale = fixture.state.clone();
        append_fixture(&mut fixture, &Fixture::with_root(60));
        let verification = vibeface_secure::signature_verification::SignatureVerification::new();
        assert!(
            verification
                .run(|| stale.accepted_journal_snapshot(&fixture.connection))
                .is_err()
        );
        assert!(
            verification
                .run(|| fixture.state.accepted_journal_snapshot(&fixture.connection))
                .is_ok()
        );
        let mut cursor = None;
        let mut hashes = Vec::new();
        for index in 0..3 {
            let page = verification
                .run(|| {
                    fixture.state.accepted_journal_page(
                        &fixture.connection,
                        &snapshot,
                        cursor.as_ref(),
                        &domains,
                    )
                })
                .unwrap();
            let retry = fixture
                .state
                .accepted_journal_page(&fixture.connection, &snapshot, cursor.as_ref(), &domains)
                .unwrap();
            assert_eq!(
                serde_json::to_value(&page.next_cursor).unwrap(),
                serde_json::to_value(&retry.next_cursor).unwrap()
            );
            assert_eq!(page.scanned, if index == 0 { 32 } else { 1 });
            assert_eq!(page.complete, index == 2);
            assert_eq!(
                page.entry.as_ref().map(|entry| &entry.entry_hash),
                retry.entry.as_ref().map(|entry| &entry.entry_hash)
            );
            if let Some(entry) = page.entry {
                assert_eq!(entry.accepted_at_ms, NOW);
                assert_eq!(entry_hash(&entry.entry).unwrap(), entry.entry_hash);
                hashes.push(entry.entry_hash);
            } else {
                assert_eq!(index, 0);
            }
            cursor = Some(page.next_cursor);
        }
        assert_eq!(hashes, selected.state.order);
        let complete = fixture
            .state
            .accepted_journal_page(&fixture.connection, &snapshot, cursor.as_ref(), &domains)
            .unwrap();
        assert!(complete.complete && complete.entry.is_none() && complete.scanned == 0);
        assert_eq!(verification.counts().actual_verifications, 0);
    }

    #[test]
    fn accepted_pages_reject_changed_unread_rows_and_cursor_boundaries() {
        for mutation in [
            "UPDATE policy_journal SET entry=entry || ' ' WHERE ordinal=1",
            "UPDATE policy_journal SET accepted_at_ms=accepted_at_ms+1 WHERE ordinal=1",
            "DELETE FROM policy_journal WHERE ordinal=1",
            "UPDATE policy_journal SET ordinal=ordinal+100; UPDATE policy_journal SET ordinal=103-ordinal",
        ] {
            let fixture = Fixture::new();
            let snapshot = fixture
                .state
                .accepted_journal_snapshot(&fixture.connection)
                .unwrap();
            fixture.connection.execute_batch(mutation).unwrap();
            assert!(
                fixture
                    .state
                    .accepted_journal_page(&fixture.connection, &snapshot, None, &BTreeSet::new(),)
                    .is_err(),
                "accepted mutation: {mutation}"
            );
        }
        let fixture = Fixture::new();
        fixture
            .connection
            .execute("UPDATE policy_journal SET ordinal=ordinal*10", [])
            .unwrap();
        let snapshot = fixture
            .state
            .accepted_journal_snapshot(&fixture.connection)
            .unwrap();
        let other = Fixture::with_root(70);
        let hash = &other.state.order[0];
        let accepted = &other.state.accepted[hash];
        fixture.connection.execute(
            "INSERT INTO policy_journal(ordinal,entry_hash,entry,accepted_at_ms) VALUES(15,?,?,?)",
            params![hash.as_str(), accepted.encoded, accepted.received_at_ms],
        ).unwrap();
        assert!(
            fixture
                .state
                .accepted_journal_page(&fixture.connection, &snapshot, None, &BTreeSet::new(),)
                .is_err()
        );

        let fixture = Fixture::new();
        let snapshot = fixture
            .state
            .accepted_journal_snapshot(&fixture.connection)
            .unwrap();
        let domains = BTreeSet::from([PolicyDomain::Account(fixture.account.clone())]);
        let page = fixture
            .state
            .accepted_journal_page(&fixture.connection, &snapshot, None, &domains)
            .unwrap();
        fixture
            .connection
            .execute(
                "UPDATE policy_journal SET entry=entry || ' ' WHERE ordinal=1",
                [],
            )
            .unwrap();
        assert!(
            fixture
                .state
                .accepted_journal_page(
                    &fixture.connection,
                    &snapshot,
                    Some(&page.next_cursor),
                    &domains,
                )
                .is_err()
        );
        let invalid = AcceptedJournalCursor {
            next_index: 3,
            after_ordinal: Some(3),
        };
        assert!(
            fixture
                .state
                .accepted_journal_page(&fixture.connection, &snapshot, Some(&invalid), &domains,)
                .is_err()
        );

        let connection = database();
        let state = PolicyState::default();
        let snapshot = state.accepted_journal_snapshot(&connection).unwrap();
        assert!(
            state
                .accepted_journal_page(&connection, &snapshot, None, &BTreeSet::new(),)
                .unwrap()
                .complete
        );
        connection
            .execute(
                "INSERT INTO policy_journal(entry_hash,entry,accepted_at_ms) VALUES(?,?,?)",
                params![hash.as_str(), accepted.encoded, accepted.received_at_ms],
            )
            .unwrap();
        assert!(state.accepted_journal_snapshot(&connection).is_err());
    }

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
            Self::with_root(1)
        }

        fn with_root(seed: u8) -> Self {
            let root = key(seed);
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
            let receipts = vec![
                Signed::sign(
                    FreshnessReceipt {
                        version: AUTH_VERSION,
                        currents: dependencies,
                        challenge_hash: commit.challenge_hash().unwrap(),
                        issued_at_ms: NOW,
                        expires_at_ms: NOW + 10_000,
                    },
                    &self.authority,
                )
                .unwrap(),
            ];
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
    fn historical_import_preserves_receipt_time_for_expired_certificates() {
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
        let unregistered = key(6);
        let mut certificate = signed.payload.clone();
        certificate.device = DeviceId::from_key(&PublicKey::of(&unregistered));
        certificate.public_key = PublicKey::of(&unregistered);
        certificate.nonce = Nonce::from_bytes([6; 32]);
        certificate.prove_possession(&unregistered).unwrap();
        let unregistered = Signed::sign(certificate, &fixture.root).unwrap();
        fixture
            .accept(&[JournalEntry::Device(unregistered.clone())], NOW)
            .unwrap();
        assert_eq!(
            fixture
                .state
                .verified()
                .device(&unregistered.hash().unwrap()),
            Some(&unregistered)
        );
        let journal: Vec<_> = fixture
            .state
            .order
            .iter()
            .map(|hash| fixture.state.accepted[hash].entry.clone())
            .collect();
        assert!(
            PolicyState::default()
                .prepare_bootstrap(fixture.state.pins(), &journal, NOW - 2)
                .is_err()
        );
        let update = PolicyState::default()
            .prepare_bootstrap(fixture.state.pins(), &journal, NOW + 100_000)
            .unwrap();
        let mut imported = database();
        let transaction = imported.transaction().unwrap();
        let imported_state = commit_in(&transaction, &update).unwrap();
        transaction.commit().unwrap();
        assert_eq!(
            imported
                .query_row(
                    "SELECT min(accepted_at_ms) FROM policy_journal",
                    [],
                    |row| { row.get::<_, u64>(0) }
                )
                .unwrap(),
            NOW + 100_000
        );
        assert_eq!(
            imported_state
                .historical(&heads, NOW)
                .unwrap()
                .device(&signed.hash().unwrap()),
            Some(&signed)
        );
        assert!(
            imported_state
                .historical(&heads, NOW)
                .unwrap()
                .device(&unregistered.hash().unwrap())
                .is_none()
        );
        assert!(
            imported_state
                .historical(&heads, NOW - 2)
                .unwrap()
                .device(&signed.hash().unwrap())
                .is_none()
        );
        assert_eq!(
            PolicyState::load(&imported)
                .unwrap()
                .verified()
                .device(&signed.hash().unwrap()),
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
    fn historical_import_replays_expired_commits_without_advancing_live_updates() {
        let mut fixture = Fixture::new();
        let change = fixture.space_commit(7);
        assert!(
            fixture
                .state
                .prepare_network_update(&[change.clone()], NOW + 100_000)
                .is_err()
        );
        fixture.accept(&[change], NOW).unwrap();
        let heads: Vec<_> = fixture.state.heads().into_values().collect();
        let journal: Vec<_> = fixture
            .state
            .order
            .iter()
            .map(|hash| fixture.state.accepted[hash].entry.clone())
            .collect();
        let mut pins = fixture.state.pins().to_vec();
        for pin in &mut pins {
            pin.minimum_head = fixture.state.verified().head(&pin.domain).cloned();
        }
        assert!(
            PolicyState::default()
                .prepare_bootstrap(&pins, &journal, NOW - 1)
                .is_err()
        );
        let mut forged = journal.clone();
        if let JournalEntry::SpaceCommit(change) = &mut forged[2] {
            change.commit.signature = Signed::sign(change.commit.payload.clone(), &key(9))
                .unwrap()
                .signature;
        }
        assert!(
            PolicyState::default()
                .prepare_bootstrap(&pins, &forged, NOW + 100_000)
                .is_err()
        );
        let mut forged = journal.clone();
        if let JournalEntry::SpaceCommit(change) = &mut forged[2] {
            change.receipts[0].signature =
                Signed::sign(change.receipts[0].payload.clone(), &key(9))
                    .unwrap()
                    .signature;
        }
        assert!(
            PolicyState::default()
                .prepare_bootstrap(&pins, &forged, NOW + 100_000)
                .is_err()
        );
        let imported = PolicyState::default()
            .prepare_bootstrap(&pins, &journal, NOW + 100_000)
            .unwrap()
            .after;
        for head in &heads {
            assert_eq!(
                imported.historical(&heads, NOW).unwrap().head(&head.domain),
                Some(head)
            );
        }
        assert!(imported.historical(&heads, NOW - 1).is_err());
        let mut wrong = heads.clone();
        wrong[0].sequence += 1;
        assert!(imported.historical(&wrong, NOW).is_err());
        assert_eq!(
            imported
                .prepare_bootstrap(&pins, &journal[..2], NOW + 100_000)
                .unwrap()
                .after
                .heads(),
            fixture.state.heads()
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
        let current: Vec<_> = fixture.state.heads().into_values().collect();
        let verification = vibeface_secure::signature_verification::SignatureVerification::new();
        verification.run(|| {
            assert!(fixture.state.current_heads_at(&current, NOW).unwrap());
            assert!(fixture.state.current_heads_at(&current, NOW - 1).is_err());
            assert!(!fixture.state.current_heads_at(&first, NOW).unwrap());
        });
        assert_eq!(verification.counts().actual_verifications, 0);
        assert!(
            fixture
                .state
                .historical(&[first[0].clone(), first[0].clone()], NOW)
                .is_err()
        );
    }

    #[test]
    fn historical_import_ignores_an_unrelated_future_commit() {
        let mut first = Fixture::new();
        first.accept(&[first.space_commit(7)], NOW).unwrap();
        let second = Fixture::with_root(8);
        let expected: Vec<_> = second.state.heads().into_values().collect();
        let journal: Vec<_> = second
            .state
            .order
            .iter()
            .map(|hash| second.state.accepted[hash].entry.clone())
            .collect();
        let imported = first
            .state
            .prepare_bootstrap(second.state.pins(), &journal, NOW + 1)
            .unwrap()
            .after;
        let historical = imported.historical(&expected, NOW - 1).unwrap();
        assert!(imported.current_heads_at(&expected, NOW - 1).unwrap());
        for head in &expected {
            assert_eq!(historical.head(&head.domain), Some(head));
        }
    }

    #[test]
    fn team_creation_requires_current_verified_source_and_a_new_domain() -> Result<()> {
        use crate::identity::test_support::{TestNetwork, now};
        use vibeface_secure::authorization::{ResourceRef, governance};

        let network = TestNetwork::new()?;
        let mut store = network.memory()?;
        let at = now();
        let audience = store
            .policy
            .verified()
            .grant(&network.bundle.grant_leaf)
            .context("source grant missing")?
            .payload
            .audience
            .clone();
        let authorization = crate::identity::client_policy_read::verified_client_policy_for_test(
            &mut store,
            &network.node,
            &network.client,
            &network.account,
            vibeface_secure::authorization::client_policy_import::AuthorizationSelection {
                resource: ResourceRef {
                    space: network.space.clone(),
                    resource: network.resource.clone(),
                },
                audience,
                grant_leaf: network.bundle.grant_leaf.clone(),
            },
            &network.pins,
            at,
        )?;
        let prepared = governance::prepare_team_creation(
            &key(8),
            &network.client,
            network.account.as_str(),
            &network.node.public_key_hex()?,
            PublicKey::of(&network.recovery).as_str(),
            network.resource.as_str(),
            &authorization,
            (at + 1_000_000) as f64,
            at as f64,
        )
        .map_err(anyhow::Error::msg)?;
        let signed = serde_json::from_str(&prepared.request_json().map_err(anyhow::Error::msg)?)?;
        let verified = governance::verify_team_creation_request(
            store.policy.verified(),
            &network.bundle.grant_leaf,
            &network.pins,
            &signed,
            at,
        )?;
        assert!(
            store
                .policy
                .prepare_team_creation(&verified, at - 1)
                .is_err()
        );
        assert!(
            store
                .policy
                .prepare_team_creation(&verified, verified.expires_at_ms())
                .is_err()
        );
        assert!(
            store
                .policy
                .prepare_network_update(&verified.request().journal_delta, at)
                .is_err()
        );
        let account_head = store
            .policy
            .verified()
            .head(&PolicyDomain::Account(network.account.clone()))
            .unwrap()
            .clone();
        let mut account = store
            .policy
            .verified()
            .account(&network.account)
            .unwrap()
            .clone();
        account.previous = Some(account_head);
        account.sequence += 1;
        let stale = store
            .policy
            .prepare_network_update(
                &[JournalEntry::Account(Signed::sign(account, &network.root)?)],
                at,
            )?
            .after;
        assert!(stale.prepare_team_creation(&verified, at).is_err());
        let update = store.policy.prepare_team_creation(&verified, at)?;
        let tx = store.connection.transaction()?;
        let accepted = commit_in(&tx, &update)?;
        tx.commit()?;
        assert!(
            accepted
                .verified()
                .space(&verified.request().team_project.space)
                .is_some()
        );
        for pin in &network.pins {
            assert!(accepted.pins().contains(pin));
        }
        assert!(accepted.prepare_team_creation(&verified, at).is_err());
        assert_eq!(
            PolicyState::load(&store.connection)?.heads(),
            accepted.heads()
        );
        Ok(())
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
    #[test]
    fn reload_verified_reuses_only_the_exact_prefix_and_verifies_new_commits() {
        use vibeface_secure::signature_verification::SignatureVerification;
        let mut fixture = Fixture::new();
        let old = fixture.state.clone();
        let no_change = SignatureVerification::new();
        let unchanged = no_change
            .run(|| PolicyState::reload_verified(&fixture.connection, &old))
            .unwrap();
        assert_eq!(unchanged.heads(), old.heads());
        assert_eq!(no_change.counts().actual_verifications, 0);
        fixture.accept(&[fixture.space_commit(21)], NOW).unwrap();
        let tail = SignatureVerification::new();
        let updated = tail
            .run(|| PolicyState::reload_verified(&fixture.connection, &old))
            .unwrap();
        assert_eq!(updated.heads(), fixture.state.heads());
        assert_eq!(updated.order, fixture.state.order);
        assert!(tail.counts().actual_verifications > 0);
        assert!(tail.counts().actual_verifications <= 32);
        assert_ne!(updated.heads(), old.heads());
        assert_eq!(
            PolicyState::load(&fixture.connection).unwrap().heads(),
            updated.heads()
        );
    }

    #[test]
    fn reload_verified_rejects_modified_removed_or_reordered_prefix_and_pins() {
        for sql in [
            "UPDATE policy_journal SET entry=entry||' ' WHERE ordinal=(SELECT MIN(ordinal) FROM policy_journal)",
            "UPDATE policy_journal SET accepted_at_ms=accepted_at_ms+1 WHERE ordinal=(SELECT MIN(ordinal) FROM policy_journal)",
            "DELETE FROM policy_journal WHERE ordinal=(SELECT MIN(ordinal) FROM policy_journal)",
            "UPDATE policy_journal SET ordinal=100 WHERE ordinal=(SELECT MIN(ordinal) FROM policy_journal)",
            "DELETE FROM policy_pins WHERE domain_kind='account'",
            "UPDATE policy_pins SET genesis_root=replace(genesis_root,'a','b') WHERE domain_kind='account'",
            "DELETE FROM current_policy_heads WHERE domain_kind='account'",
        ] {
            let fixture = Fixture::new();
            fixture.connection.execute_batch(sql).unwrap();
            assert!(
                PolicyState::reload_verified(&fixture.connection, &fixture.state).is_err(),
                "{sql}"
            );
        }
        let fixture = Fixture::new();
        let pin = &fixture.state.pins()[0];
        let head = fixture.state.verified().head(&pin.domain).unwrap();
        let (kind, id) = domain_key(&pin.domain);
        fixture
            .connection
            .execute(
                "UPDATE policy_pins SET minimum_head=? WHERE domain_kind=? AND domain_id=?",
                params![serde_json::to_string(head).unwrap(), kind, id],
            )
            .unwrap();
        assert!(PolicyState::reload_verified(&fixture.connection, &fixture.state).is_err());
    }

    #[test]
    fn reload_verified_rejects_forged_tail_and_missing_commit_nonce() {
        let mut fixture = Fixture::new();
        let old = fixture.state.clone();
        fixture.accept(&[fixture.space_commit(22)], NOW).unwrap();
        fixture
            .connection
            .execute("DELETE FROM policy_nonces", [])
            .unwrap();
        assert!(PolicyState::reload_verified(&fixture.connection, &old).is_err());
        let fixture = Fixture::new();
        let mut change = fixture.space_commit(23);
        if let JournalEntry::SpaceCommit(change) = &mut change {
            change.commit.signature = Signed::sign(change.commit.payload.clone(), &key(9))
                .unwrap()
                .signature;
        }
        fixture
            .connection
            .execute(
                "INSERT INTO policy_journal(entry_hash,entry,accepted_at_ms) VALUES(?,?,?)",
                params![
                    entry_hash(&change).unwrap().as_str(),
                    serde_json::to_string(&change).unwrap(),
                    NOW
                ],
            )
            .unwrap();
        assert!(PolicyState::reload_verified(&fixture.connection, &fixture.state).is_err());
        assert_eq!(fixture.state.order.len(), 2);
    }
    #[test]
    fn reload_verified_observes_another_connection_and_preserves_conflict_rollback() {
        let fixture = Fixture::new();
        let uri = format!(
            "file:journal-reload-{}?mode=memory&cache=shared",
            uuid::Uuid::new_v4()
        );
        let mut writer = Connection::open(&uri).unwrap();
        writer
            .execute_batch(include_str!("../../migrations/202610180001_initial.sql"))
            .unwrap();
        let entries: Vec<_> = fixture
            .state
            .order
            .iter()
            .map(|hash| fixture.state.accepted[hash].entry.clone())
            .collect();
        let initial = PolicyState::default()
            .prepare_bootstrap(fixture.state.pins(), &entries, NOW)
            .unwrap();
        let tx = writer.transaction().unwrap();
        let known = commit_in(&tx, &initial).unwrap();
        tx.commit().unwrap();
        let mut reader = Connection::open(&uri).unwrap();
        let next = known
            .prepare_network_update(&[fixture.space_commit(24)], NOW)
            .unwrap();
        let stale = known
            .prepare_network_update(&[fixture.space_commit(25)], NOW)
            .unwrap();
        let tx = writer
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .unwrap();
        let changed = commit_in(&tx, &next).unwrap();
        tx.commit().unwrap();
        let scope = vibeface_secure::signature_verification::SignatureVerification::new();
        let tx = reader.transaction().unwrap();
        let observed = scope
            .run(|| PolicyState::reload_verified(&tx, &known))
            .unwrap();
        assert_eq!(observed.heads(), changed.heads());
        assert_ne!(observed.heads(), known.heads());
        assert!(scope.counts().actual_verifications > 0);
        tx.commit().unwrap();
        let tx = writer
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .unwrap();
        assert!(commit_in(&tx, &stale).is_err());
        tx.rollback().unwrap();
        let after = PolicyState::load(&reader).unwrap();
        assert_eq!(after.heads(), changed.heads());
        assert_eq!(after.order, changed.order);
        assert_eq!(
            after.nonce_bindings().unwrap(),
            changed.nonce_bindings().unwrap()
        );
    }
}
