//! Atomic passkey pending records, policy changes, counters, and retry receipts.

use super::journal::{self, PolicyState, PreparedPolicyUpdate};
use crate::store::Store;
use anyhow::{Context, Result, bail, ensure};
use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use vibeface_secure::{
    authorization::{
        AUTH_VERSION, AccountDocument, AccountId, AccountRight, AuthenticatorManagementIntent,
        AuthenticatorRecord, DeviceId, JournalEntry, Nonce, ObjectHash, PolicyDomain, PolicyStamp,
        PublicKey, Signed, VerifiedPolicySet,
        enrollment::{
            AuthenticatorEnrollmentIntent, DeviceEnrollmentIntent, verify_authenticator_enrollment,
            verify_passkey_device_enrollment, verify_passkey_enrollment_request,
        },
        passkey_node::NodePasskeyState,
    },
    membership::canonical_json,
    webauthn::{
        AssertionResponse, AuthenticationCeremony, AuthenticatorState, EnrollmentEnvelope,
        PASSKEY_MAX_AGE_MS, PasskeyAssertionProof, PasskeyPurpose, PendingPasskey,
        RegistrationResponse, RelyingParty, VerifiedPasskeyAssertion, verify_passkey_registration,
    },
};

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum PasskeyReceiptKind {
    AuthenticatorRegistered {
        credential: PasskeyCredentialReceipt,
    },
    DeviceEnrolled {
        device: DeviceId,
        credential: PasskeyCredentialReceipt,
    },
    AuthenticatorsManaged {
        credential: PasskeyCredentialReceipt,
    },
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PasskeyCredentialReceipt {
    pub credential_id: String,
    pub record_hash: ObjectHash,
    pub state: AuthenticatorState,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PasskeyReceipt {
    pub version: u16,
    pub account: AccountId,
    pub envelope_hash: ObjectHash,
    pub completion_hash: ObjectHash,
    pub kind: PasskeyReceiptKind,
    pub journal: Vec<JournalEntry>,
    pub accepted_at_ms: u64,
}

pub(crate) struct PasskeyContext {
    pub(crate) target: DeviceId,
    pub(crate) relying_party: RelyingParty,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PasskeyRegistrationCompletion {
    pub intent: Signed<AuthenticatorEnrollmentIntent>,
    pub pending_id: ObjectHash,
    pub response: RegistrationResponse,
    pub journal: Vec<JournalEntry>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PasskeyDeviceCompletion {
    pub intent: Signed<DeviceEnrollmentIntent>,
    pub pending_id: ObjectHash,
    pub journal: Vec<JournalEntry>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PasskeyManagementCompletion {
    pub intent: Signed<AuthenticatorManagementIntent>,
    pub pending_id: ObjectHash,
    pub journal: Vec<JournalEntry>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum PasskeyPreparation {
    Pending {
        pending_id: ObjectHash,
        pending: PendingPasskey,
        options: serde_json::Value,
    },
    Completed {
        receipt: PasskeyReceipt,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct LedgerSnapshot {
    rp_id: String,
    record_hash: ObjectHash,
    state: AuthenticatorState,
    last_envelope_hash: ObjectHash,
}
struct LedgerMutation {
    credential_id: String,
    rp_id: String,
    record_hash: ObjectHash,
    before: Option<LedgerSnapshot>,
    after: AuthenticatorState,
    envelope_hash: ObjectHash,
}

pub(crate) struct VerifiedPasskeyResponse {
    proof: PasskeyAssertionProof,
    verified: VerifiedPasskeyAssertion,
    ledger: Option<LedgerSnapshot>,
}
impl VerifiedPasskeyResponse {
    pub(crate) fn proof(&self) -> &PasskeyAssertionProof {
        &self.proof
    }
    pub(crate) fn verified(&self) -> &VerifiedPasskeyAssertion {
        &self.verified
    }
    pub(crate) fn node_state(&self, now_ms: u64) -> Result<NodePasskeyState> {
        ensure!(
            now_ms < self.proof.pending.envelope().expires_at_ms,
            "verified passkey response expired before node attestation"
        );
        Ok(NodePasskeyState {
            version: AUTH_VERSION,
            pending: self.proof.pending.clone(),
            credential_id: self.verified.credential_id().to_owned(),
            record_hash: self.verified.record_hash().clone(),
            state: self.verified.previous_state(),
            response_hash: ObjectHash::body(&self.proof.response)?,
            issued_at_ms: now_ms,
            expires_at_ms: self.proof.pending.envelope().expires_at_ms,
        })
    }
}

pub(crate) struct PreparedPasskeyCompletion {
    pending: Vec<PendingPasskey>,
    update: PreparedPolicyUpdate,
    ledger: Vec<LedgerMutation>,
    receipts: Vec<PasskeyReceipt>,
    qualification_expires_at_ms: u64,
}

fn canonical<T: Serialize>(value: &T) -> Result<String> {
    String::from_utf8(canonical_json(value).map_err(anyhow::Error::msg)?)
        .context("invalid passkey canonical JSON")
}

fn require_current_heads(
    policies: &VerifiedPolicySet,
    envelope: &EnrollmentEnvelope,
) -> Result<()> {
    ensure!(
        envelope
            .expected_heads
            .iter()
            .any(|head| head.domain == PolicyDomain::Account(envelope.account.clone())),
        "passkey request is missing its account policy head"
    );
    for expected in &envelope.expected_heads {
        ensure!(
            policies.head(&expected.domain) == Some(expected),
            "passkey policy changed"
        );
    }
    Ok(())
}

fn require_sql_heads(connection: &Connection, expected_heads: &[PolicyStamp]) -> Result<()> {
    for expected in expected_heads {
        let (kind, id) = match &expected.domain {
            PolicyDomain::Account(id) => ("account", id.as_str()),
            PolicyDomain::Space(id) => ("space", id.as_str()),
        };
        let actual:(u64,u64,String)=connection.query_row("SELECT authority_epoch,sequence,head_hash FROM current_policy_heads WHERE domain_kind=? AND domain_id=?",params![kind,id],|row|Ok((row.get(0)?,row.get(1)?,row.get(2)?)))?;
        ensure!(
            actual
                == (
                    expected.authority_epoch,
                    expected.sequence,
                    expected.head.as_str().to_owned()
                ),
            "passkey policy changed in the database"
        );
    }
    Ok(())
}

fn read_pending(
    connection: &Connection,
    account: &AccountId,
    envelope_hash: &ObjectHash,
) -> Result<PendingPasskey> {
    let (intent_hash, nonce, encoded, expiry): (String, String, String, u64) = connection.query_row(
        "SELECT intent_hash,nonce,pending,expires_at_ms FROM passkey_pending WHERE account_id=? AND envelope_hash=?",
        params![account.as_str(), envelope_hash.as_str()],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
    )?;
    ensure!(
        encoded.len() <= 65_536,
        "stored passkey pending is too large"
    );
    let pending: PendingPasskey = serde_json::from_str(&encoded)?;
    let envelope = pending.envelope();
    ensure!(
        envelope.version == AUTH_VERSION
            && envelope.account == *account
            && pending.envelope_hash().map_err(anyhow::Error::msg)? == *envelope_hash
            && envelope.enrollment_hash.as_str() == intent_hash
            && envelope.nonce.as_str() == nonce
            && envelope.expires_at_ms == expiry
            && envelope.not_before_ms < expiry
            && expiry - envelope.not_before_ms <= PASSKEY_MAX_AGE_MS
            && canonical(&pending)? == encoded,
        "stored passkey pending does not match its immutable binding"
    );
    RelyingParty::new(
        pending.relying_party().rp_id(),
        pending.relying_party().origin(),
    )
    .map_err(anyhow::Error::msg)?;
    Ok(pending)
}

fn read_receipt(
    connection: &Connection,
    account: &AccountId,
    envelope_hash: &ObjectHash,
    completion_hash: Option<&ObjectHash>,
) -> Result<Option<PasskeyReceipt>> {
    let row: Option<(String, String, u64)> = connection.query_row(
        "SELECT completion_hash,receipt,accepted_at_ms FROM passkey_receipts WHERE account_id=? AND envelope_hash=?",
        params![account.as_str(), envelope_hash.as_str()],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
    ).optional()?;
    let Some((stored_hash, encoded, accepted_at_ms)) = row else {
        return Ok(None);
    };
    if let Some(expected) = completion_hash {
        ensure!(
            stored_hash == expected.as_str(),
            "passkey request was consumed by another completion"
        );
    }
    let receipt: PasskeyReceipt = serde_json::from_str(&encoded)?;
    ensure!(
        receipt.version == AUTH_VERSION
            && receipt.account == *account
            && receipt.envelope_hash == *envelope_hash
            && receipt.completion_hash.as_str() == stored_hash
            && receipt.accepted_at_ms == accepted_at_ms
            && canonical(&receipt)? == encoded,
        "stored passkey receipt binding is invalid"
    );
    Ok(Some(receipt))
}

fn read_ledger(
    connection: &Connection,
    account: &AccountId,
    credential_id: &str,
) -> Result<Option<LedgerSnapshot>> {
    let row: Option<(String, String, u32, u8, String)> = connection.query_row(
        "SELECT rp_id,record_hash,sign_count,backup_state,last_envelope_hash FROM passkey_authenticators WHERE account_id=? AND credential_id=?",
        params![account.as_str(), credential_id],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?)),
    ).optional()?;
    row.map(
        |(rp_id, record_hash, sign_count, backup_state, last_envelope_hash)| {
            ensure!(backup_state <= 1, "invalid stored passkey backup state");
            Ok(LedgerSnapshot {
                rp_id,
                record_hash: ObjectHash::parse(&record_hash)?,
                state: AuthenticatorState {
                    sign_count,
                    backup_state: backup_state == 1,
                },
                last_envelope_hash: ObjectHash::parse(&last_envelope_hash)?,
            })
        },
    )
    .transpose()
}

fn ledger_state(snapshot: &Option<LedgerSnapshot>) -> AuthenticatorState {
    snapshot
        .as_ref()
        .map(|snapshot| snapshot.state)
        .unwrap_or(AuthenticatorState {
            sign_count: 0,
            backup_state: false,
        })
}

fn write_ledger_in(
    tx: &Transaction<'_>,
    account: &AccountId,
    mutation: &LedgerMutation,
) -> Result<()> {
    let current = read_ledger(tx, account, &mutation.credential_id)?;
    ensure!(
        current == mutation.before,
        "passkey counter changed concurrently"
    );
    if let Some(before) = &mutation.before {
        ensure!(before.rp_id == mutation.rp_id, "passkey RP cannot change");
        let changed = tx.execute(
            "UPDATE passkey_authenticators SET record_hash=?,sign_count=?,backup_state=?,last_envelope_hash=? WHERE account_id=? AND credential_id=? AND record_hash=? AND sign_count=? AND backup_state=? AND last_envelope_hash=?",
            params![mutation.record_hash.as_str(), mutation.after.sign_count, mutation.after.backup_state, mutation.envelope_hash.as_str(), account.as_str(), mutation.credential_id, before.record_hash.as_str(), before.state.sign_count, before.state.backup_state, before.last_envelope_hash.as_str()],
        )?;
        ensure!(changed == 1, "passkey counter compare-and-swap failed");
    } else {
        tx.execute(
            "INSERT INTO passkey_authenticators(account_id,credential_id,rp_id,record_hash,sign_count,backup_state,last_envelope_hash) VALUES(?,?,?,?,?,?,?)",
            params![account.as_str(), mutation.credential_id, mutation.rp_id, mutation.record_hash.as_str(), mutation.after.sign_count, mutation.after.backup_state, mutation.envelope_hash.as_str()],
        )?;
    }
    Ok(())
}

fn preparation(
    connection: &Connection,
    pending: PendingPasskey,
    now_ms: u64,
) -> Result<PasskeyPreparation> {
    let id = pending.envelope_hash().map_err(anyhow::Error::msg)?;
    if let Some(receipt) = read_receipt(connection, &pending.envelope().account, &id, None)? {
        return Ok(PasskeyPreparation::Completed { receipt });
    }
    ensure!(
        pending.envelope().not_before_ms <= now_ms && now_ms < pending.envelope().expires_at_ms,
        "passkey request expired; create a new signed intent"
    );
    let options = match pending.envelope().purpose {
        PasskeyPurpose::RegisterAuthenticator => pending.creation_options(),
        PasskeyPurpose::EnrollDevice | PasskeyPurpose::ManageAuthenticators => {
            pending.authentication_options()
        }
    }
    .map_err(anyhow::Error::msg)?;
    Ok(PasskeyPreparation::Pending {
        pending_id: id,
        pending,
        options,
    })
}

fn persist_pending(
    connection: &mut Connection,
    policy: &PolicyState,
    pending: PendingPasskey,
    now_ms: u64,
) -> Result<PasskeyPreparation> {
    require_current_heads(policy.verified(), pending.envelope())?;
    let snapshot = policy.prepare_network_update(&[], now_ms)?;
    let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    journal::commit_in(&tx, &snapshot)?;
    let old: Option<String> = tx
        .query_row(
            "SELECT envelope_hash FROM passkey_pending WHERE account_id=? AND intent_hash=?",
            params![
                pending.envelope().account.as_str(),
                pending.envelope().enrollment_hash.as_str()
            ],
            |row| row.get(0),
        )
        .optional()?;
    if let Some(old) = old {
        let old = read_pending(&tx, &pending.envelope().account, &ObjectHash::parse(&old)?)?;
        ensure!(
            old.relying_party() == pending.relying_party()
                && old.envelope().purpose == pending.envelope().purpose,
            "same passkey intent cannot change its ceremony or RP"
        );
        let result = preparation(&tx, old, now_ms)?;
        tx.commit()?;
        return Ok(result);
    }
    let active: u64 = tx.query_row(
        "SELECT count(*) FROM passkey_pending p WHERE p.account_id=? AND p.expires_at_ms>? AND NOT EXISTS(SELECT 1 FROM passkey_receipts r WHERE r.account_id=p.account_id AND r.envelope_hash=p.envelope_hash)",
        params![pending.envelope().account.as_str(), now_ms], |row| row.get(0),
    )?;
    ensure!(active < 32, "too many pending passkey requests");
    let id = pending.envelope_hash().map_err(anyhow::Error::msg)?;
    let encoded = canonical(&pending)?;
    ensure!(encoded.len() <= 65_536, "passkey pending is too large");
    tx.execute(
        "INSERT INTO passkey_pending(account_id,envelope_hash,intent_hash,nonce,pending,expires_at_ms) VALUES(?,?,?,?,?,?)",
        params![pending.envelope().account.as_str(), id.as_str(), pending.envelope().enrollment_hash.as_str(), pending.envelope().nonce.as_str(), encoded, pending.envelope().expires_at_ms],
    )?;
    let result = preparation(&tx, pending, now_ms)?;
    tx.commit()?;
    Ok(result)
}

fn account_user_handle(account: &AccountId) -> [u8; 32] {
    let mut hash = Sha256::new();
    hash.update(b"vibeface:passkey-user:v2\0");
    hash.update(account.as_str().as_bytes());
    hash.finalize().into()
}

fn intent_retry(
    connection: &Connection,
    account: &AccountId,
    intent_hash: &ObjectHash,
    issuer: &PublicKey,
    context: &PasskeyContext,
    purpose: PasskeyPurpose,
    now_ms: u64,
) -> Result<Option<PasskeyPreparation>> {
    let id: Option<String> = connection
        .query_row(
            "SELECT envelope_hash FROM passkey_pending WHERE account_id=? AND intent_hash=?",
            params![account.as_str(), intent_hash.as_str()],
            |row| row.get(0),
        )
        .optional()?;
    let Some(id) = id else {
        return Ok(None);
    };
    let id = ObjectHash::parse(&id)?;
    let pending = read_pending(connection, account, &id)?;
    ensure!(
        pending.envelope().target == context.target
            && pending.relying_party() == &context.relying_party
            && pending.envelope().purpose == purpose,
        "passkey intent cannot change its target, RP, or purpose"
    );
    if let Some(receipt) = read_receipt(connection, account, &id, None)? {
        let proposal = receipt
            .journal
            .iter()
            .rev()
            .find_map(|entry| match entry {
                JournalEntry::AccountCommit(committed) => Some(&committed.proposal),
                _ => None,
            })
            .context("passkey receipt is missing its committed account policy")?;
        let accepted_issuer = match purpose {
            PasskeyPurpose::RegisterAuthenticator => &proposal.issuer,
            PasskeyPurpose::EnrollDevice => {
                &proposal
                    .payload
                    .device_enrollment
                    .as_ref()
                    .context("passkey receipt is missing its enrollment intent")?
                    .intent
                    .issuer
            }
            PasskeyPurpose::ManageAuthenticators => {
                &proposal
                    .payload
                    .authenticator_approval
                    .as_ref()
                    .context("passkey receipt is missing its management intent")?
                    .intent
                    .issuer
            }
        };
        ensure!(
            issuer == accepted_issuer,
            "passkey retry was signed by another actor"
        );
        return Ok(Some(PasskeyPreparation::Completed { receipt }));
    }
    ensure!(
        now_ms < pending.envelope().expires_at_ms,
        "passkey request expired; create a new signed intent"
    );
    Ok(None)
}

fn request_pending(
    mut envelope: EnrollmentEnvelope,
    context: &PasskeyContext,
    expires_at_ms: u64,
    credential_ids: Vec<String>,
    ceremony: AuthenticationCeremony,
    now_ms: u64,
) -> Result<PendingPasskey> {
    ensure!(
        envelope.target == context.target,
        "passkey target is not this node"
    );
    envelope.not_before_ms = now_ms;
    envelope.expires_at_ms = expires_at_ms.min(now_ms.saturating_add(PASSKEY_MAX_AGE_MS));
    let handle = account_user_handle(&envelope.account);
    PendingPasskey::prepare(
        envelope,
        context.relying_party.clone(),
        &handle,
        ceremony,
        credential_ids,
        now_ms,
    )
    .map_err(anyhow::Error::msg)
}

fn account_proposal(entry: &JournalEntry) -> Result<&Signed<AccountDocument>> {
    match entry {
        JournalEntry::AccountCommit(committed) => Ok(&committed.proposal),
        _ => bail!("passkey completion must contain one account policy update"),
    }
}

fn registration_delta<'a>(
    policies: &VerifiedPolicySet,
    pending: &PendingPasskey,
    journal: &'a [JournalEntry],
) -> Result<&'a Signed<AuthenticatorRecord>> {
    ensure!(
        journal.len() == 2,
        "passkey registration must contain exactly two policy entries"
    );
    let JournalEntry::Authenticator(record) = &journal[0] else {
        bail!("passkey registration is missing its signed authenticator");
    };
    let proposal = account_proposal(&journal[1])?;
    let before = policies
        .account(&pending.envelope().account)
        .context("passkey account is missing")?;
    let mut expected = before.clone();
    expected.device_enrollment = None;
    expected.authenticator_approval = proposal.payload.authenticator_approval.clone();
    expected.root_acceptance = None;
    expected.recovery_approvals.clear();
    expected.previous = policies
        .head(&PolicyDomain::Account(before.account.clone()))
        .cloned();
    expected.sequence = expected
        .sequence
        .checked_add(1)
        .context("account sequence overflow")?;
    let hash = record.hash()?;
    ensure!(
        !expected.authenticators.contains(&hash),
        "authenticator is already registered"
    );
    expected.authenticators.push(hash);
    expected.authenticators.sort();
    ensure!(
        proposal.payload == expected,
        "passkey registration changed unrelated account policy"
    );
    ensure!(
        record.payload.issued_under == *expected.previous.as_ref().unwrap()
            && record.payload.nonce == pending.envelope().nonce,
        "authenticator record is not bound to this pending request"
    );
    Ok(record)
}

fn completion_hash<T: Serialize>(completion: &T) -> Result<ObjectHash> {
    ensure!(
        canonical_json(completion)
            .map_err(anyhow::Error::msg)?
            .len()
            <= 262_144,
        "passkey completion is too large"
    );
    Ok(ObjectHash::body(completion)?)
}

fn completion_deadline(journal: &[JournalEntry], qualification: u64) -> u64 {
    journal.iter().fold(qualification, |deadline, entry| {
        let JournalEntry::AccountCommit(committed) = entry else {
            return deadline;
        };
        committed.receipts.iter().fold(
            deadline.min(committed.commit.payload.expires_at_ms),
            |deadline, receipt| deadline.min(receipt.payload.expires_at_ms),
        )
    })
}

fn completion_pending(
    connection: &Connection,
    account: &AccountId,
    pending_id: &ObjectHash,
    context: &PasskeyContext,
    purpose: PasskeyPurpose,
) -> Result<PendingPasskey> {
    let pending = read_pending(connection, account, pending_id)?;
    ensure!(
        pending.envelope().target == context.target
            && pending.relying_party() == &context.relying_party
            && pending.envelope().purpose == purpose,
        "passkey completion has the wrong target, RP, or purpose"
    );
    Ok(pending)
}

fn assertion_ledger(
    response: &VerifiedPasskeyResponse,
) -> Result<(LedgerMutation, PasskeyCredentialReceipt)> {
    let verified = response.verified();
    ensure!(
        verified.previous_state() == ledger_state(&response.ledger),
        "passkey assertion did not use the current ledger"
    );
    let state = AuthenticatorState {
        sign_count: verified.sign_count(),
        backup_state: verified.backup_state(),
    };
    Ok((
        LedgerMutation {
            credential_id: verified.credential_id().to_owned(),
            rp_id: response.proof.pending.relying_party().rp_id().to_owned(),
            record_hash: verified.record_hash().clone(),
            before: response.ledger.clone(),
            after: state,
            envelope_hash: verified.envelope_hash().map_err(anyhow::Error::msg)?,
        },
        PasskeyCredentialReceipt {
            credential_id: verified.credential_id().to_owned(),
            record_hash: verified.record_hash().clone(),
            state,
        },
    ))
}

impl Store {
    pub(crate) fn reject_passkey_policy_bypass(
        &self,
        entries: &[JournalEntry],
        local_target: &DeviceId,
    ) -> Result<()> {
        for entry in entries {
            let (hash, document) = match entry {
                JournalEntry::Authenticator(record) => (record.hash()?, None),
                JournalEntry::AccountCommit(committed) => (
                    committed.proposal.hash()?,
                    Some(&committed.proposal.payload),
                ),
                JournalEntry::Account(proposal) => (proposal.hash()?, Some(&proposal.payload)),
                _ => continue,
            };
            let accepted: bool = self.connection.query_row(
                "SELECT EXISTS(SELECT 1 FROM policy_journal WHERE entry_hash=?)",
                params![hash.as_str()],
                |row| row.get(0),
            )?;
            if accepted {
                continue;
            }
            let Some(document) = document else {
                bail!("new authenticators require atomic passkey registration completion");
            };
            let previous = self.policy.verified().account(&document.account);
            let new_device = document.device_enrollment.as_ref().filter(|proof| {
                previous.and_then(|d| d.device_enrollment.as_ref()) != Some(*proof)
            });
            let new_approval = document.authenticator_approval.as_ref().filter(|proof| {
                previous.and_then(|d| d.authenticator_approval.as_ref()) != Some(*proof)
            });
            let target = new_device
                .map(|proof| &proof.assertion.pending.envelope().target)
                .or_else(|| new_approval.map(|proof| &proof.assertion.pending.envelope().target));
            if let Some(target) = target {
                ensure!(
                    target == local_target,
                    "new passkey policy belongs to another node; use authenticated history import"
                );
                bail!("new passkey policy requires atomic passkey completion");
            }
        }
        Ok(())
    }

    pub(crate) fn prepare_passkey_registration(
        &mut self,
        intent: &Signed<AuthenticatorEnrollmentIntent>,
        context: &PasskeyContext,
        now_ms: u64,
    ) -> Result<PasskeyPreparation> {
        intent.verify_signature()?;
        let hash = ObjectHash::body(&intent.payload)?;
        if let Some(result) = intent_retry(
            &self.connection,
            &intent.payload.account,
            &hash,
            &intent.issuer,
            context,
            PasskeyPurpose::RegisterAuthenticator,
            now_ms,
        )? {
            return Ok(result);
        }
        let qualification =
            verify_authenticator_enrollment(self.policy.verified(), intent, now_ms)?;
        let i = qualification.intent();
        let account = self
            .policy
            .verified()
            .account(&i.account)
            .context("passkey account is missing")?;
        let mut credential_ids: Vec<_> = account
            .authenticators
            .iter()
            .filter_map(|hash| {
                self.policy
                    .verified()
                    .authenticator(hash)
                    .filter(|record| record.payload.rp_id == context.relying_party.rp_id())
                    .map(|record| record.payload.credential_id.clone())
            })
            .collect();
        credential_ids.sort();
        credential_ids.dedup();
        let pending = request_pending(
            EnrollmentEnvelope {
                version: AUTH_VERSION,
                purpose: PasskeyPurpose::RegisterAuthenticator,
                account: i.account.clone(),
                device_public_key: i.requesting_device_public_key.clone(),
                rights: vec![AccountRight::ManageAuthenticators],
                target: i.target.clone(),
                enrollment_hash: qualification.intent_hash().clone(),
                expected_heads: i.expected_heads.clone(),
                nonce: Nonce::from_bytes([0; 32]),
                not_before_ms: now_ms,
                expires_at_ms: qualification.expires_at_ms(),
            },
            context,
            qualification.expires_at_ms(),
            credential_ids,
            AuthenticationCeremony::IdentifiedAccount,
            now_ms,
        )?;
        persist_pending(&mut self.connection, &self.policy, pending, now_ms)
    }

    pub(crate) fn prepare_passkey_device(
        &mut self,
        intent: &Signed<DeviceEnrollmentIntent>,
        context: &PasskeyContext,
        now_ms: u64,
    ) -> Result<PasskeyPreparation> {
        intent.verify_signature()?;
        let hash = ObjectHash::body(&intent.payload)?;
        if let Some(result) = intent_retry(
            &self.connection,
            &intent.payload.account,
            &hash,
            &intent.issuer,
            context,
            PasskeyPurpose::EnrollDevice,
            now_ms,
        )? {
            return Ok(result);
        }
        let qualification =
            verify_passkey_enrollment_request(self.policy.verified(), intent, now_ms)?;
        let i = qualification.intent();
        let pending = request_pending(
            EnrollmentEnvelope {
                version: AUTH_VERSION,
                purpose: PasskeyPurpose::EnrollDevice,
                account: i.account.clone(),
                device_public_key: i.new_device_public_key.clone(),
                rights: i.requested_rights.clone(),
                target: i.target.clone(),
                enrollment_hash: qualification.intent_hash().clone(),
                expected_heads: i.expected_heads.clone(),
                nonce: Nonce::from_bytes([0; 32]),
                not_before_ms: now_ms,
                expires_at_ms: qualification.expires_at_ms(),
            },
            context,
            qualification.expires_at_ms(),
            vec![],
            AuthenticationCeremony::Discoverable,
            now_ms,
        )?;
        persist_pending(&mut self.connection, &self.policy, pending, now_ms)
    }

    pub(crate) fn prepare_passkey_management(
        &mut self,
        intent: &Signed<AuthenticatorManagementIntent>,
        context: &PasskeyContext,
        now_ms: u64,
    ) -> Result<PasskeyPreparation> {
        intent.verify_signature()?;
        let hash = ObjectHash::body(&intent.payload)?;
        if let Some(result) = intent_retry(
            &self.connection,
            &intent.payload.account,
            &hash,
            &intent.issuer,
            context,
            PasskeyPurpose::ManageAuthenticators,
            now_ms,
        )? {
            return Ok(result);
        }
        let qualification = self
            .policy
            .verified()
            .verify_authenticator_management_intent(intent, now_ms)?;
        let i = qualification.intent();
        let pending = request_pending(
            EnrollmentEnvelope {
                version: AUTH_VERSION,
                purpose: PasskeyPurpose::ManageAuthenticators,
                account: i.account.clone(),
                device_public_key: i.device_public_key.clone(),
                rights: vec![AccountRight::ManageAuthenticators],
                target: i.target.clone(),
                enrollment_hash: qualification.intent_hash().clone(),
                expected_heads: i.expected_heads.clone(),
                nonce: Nonce::from_bytes([0; 32]),
                not_before_ms: now_ms,
                expires_at_ms: qualification.expires_at_ms(),
            },
            context,
            qualification.expires_at_ms(),
            vec![],
            AuthenticationCeremony::Discoverable,
            now_ms,
        )?;
        persist_pending(&mut self.connection, &self.policy, pending, now_ms)
    }

    fn management_approval(
        &self,
        proposal: &Signed<AccountDocument>,
        context: &PasskeyContext,
        now_ms: u64,
    ) -> Result<Option<(VerifiedPasskeyResponse, u64)>> {
        let Some(approval) = &proposal.payload.authenticator_approval else {
            return Ok(None);
        };
        let qualification = self
            .policy
            .verified()
            .verify_authenticator_management_intent(&approval.intent, now_ms)?;
        let id = approval
            .assertion
            .pending
            .envelope_hash()
            .map_err(anyhow::Error::msg)?;
        let pending = completion_pending(
            &self.connection,
            &proposal.payload.account,
            &id,
            context,
            PasskeyPurpose::ManageAuthenticators,
        )?;
        ensure!(
            pending == approval.assertion.pending,
            "authenticator management changed its trusted pending"
        );
        let response = self.verify_passkey_response(
            &proposal.payload.account,
            &id,
            &serde_json::to_string(&approval.assertion.response)?,
            context,
            now_ms,
        )?;
        ensure!(
            response.proof() == &approval.assertion,
            "authenticator management changed its trusted credential state"
        );
        let record_expiry = self
            .policy
            .verified()
            .verify_authenticator(&approval.assertion.record, now_ms)?
            .expires_at_ms();
        Ok(Some((
            response,
            qualification.expires_at_ms().min(record_expiry),
        )))
    }

    pub(crate) fn complete_passkey_registration(
        &mut self,
        completion: &PasskeyRegistrationCompletion,
        context: &PasskeyContext,
        now_ms: u64,
    ) -> Result<PasskeyReceipt> {
        completion.intent.verify_signature()?;
        let account = &completion.intent.payload.account;
        let hash = completion_hash(completion)?;
        completion_pending(
            &self.connection,
            account,
            &completion.pending_id,
            context,
            PasskeyPurpose::RegisterAuthenticator,
        )?;
        if let Some(receipt) = read_receipt(
            &self.connection,
            account,
            &completion.pending_id,
            Some(&hash),
        )? {
            return Ok(receipt);
        }
        let prepared =
            self.prepare_passkey_registration_completion(completion, context, hash, now_ms)?;
        self.commit_passkey_completion(prepared, now_ms)
    }

    fn prepare_passkey_registration_completion(
        &self,
        completion: &PasskeyRegistrationCompletion,
        context: &PasskeyContext,
        completion_hash: ObjectHash,
        now_ms: u64,
    ) -> Result<PreparedPasskeyCompletion> {
        let account = &completion.intent.payload.account;
        let pending = completion_pending(
            &self.connection,
            account,
            &completion.pending_id,
            context,
            PasskeyPurpose::RegisterAuthenticator,
        )?;
        let qualified =
            verify_authenticator_enrollment(self.policy.verified(), &completion.intent, now_ms)?;
        let intent = qualified.intent();
        let envelope = pending.envelope();
        ensure!(
            envelope.enrollment_hash == *qualified.intent_hash()
                && envelope.device_public_key == intent.requesting_device_public_key
                && envelope.target == intent.target
                && envelope.rights == vec![AccountRight::ManageAuthenticators]
                && envelope.expected_heads == intent.expected_heads,
            "passkey registration changed its approved intent"
        );
        require_current_heads(self.policy.verified(), envelope)?;
        let record = registration_delta(self.policy.verified(), &pending, &completion.journal)?;
        ensure!(
            account_proposal(&completion.journal[1])?.issuer == completion.intent.issuer,
            "passkey registration completion changed its approving actor"
        );
        ensure!(
            record.payload.not_before_ms == intent.credential_not_before_ms
                && record.payload.expires_at_ms == intent.credential_expires_at_ms,
            "passkey registration changed its approved credential lifetime"
        );
        let registration = verify_passkey_registration(&pending, &completion.response, now_ms)
            .map_err(anyhow::Error::msg)?;
        let management =
            self.management_approval(account_proposal(&completion.journal[1])?, context, now_ms)?;
        let update = self
            .policy
            .prepare_network_update(&completion.journal, now_ms)?;
        let authorized = update
            .verified_after()
            .verify_authenticator(record, now_ms)?;
        registration
            .matches_authorized_record(&authorized)
            .map_err(anyhow::Error::msg)?;
        ensure!(
            read_ledger(&self.connection, account, registration.credential_id())?.is_none(),
            "passkey credential is already registered in the ledger"
        );
        let state = AuthenticatorState {
            sign_count: registration.sign_count(),
            backup_state: registration.backup_state(),
        };
        let credential = PasskeyCredentialReceipt {
            credential_id: registration.credential_id().to_owned(),
            record_hash: authorized.record_hash().clone(),
            state,
        };
        let mut result = PreparedPasskeyCompletion {
            pending: vec![pending.clone()],
            update,
            ledger: vec![LedgerMutation {
                credential_id: registration.credential_id().to_owned(),
                rp_id: registration.relying_party().rp_id().to_owned(),
                record_hash: authorized.record_hash().clone(),
                before: None,
                after: state,
                envelope_hash: completion.pending_id.clone(),
            }],
            receipts: vec![PasskeyReceipt {
                version: AUTH_VERSION,
                account: account.clone(),
                envelope_hash: completion.pending_id.clone(),
                completion_hash: completion_hash.clone(),
                kind: PasskeyReceiptKind::AuthenticatorRegistered { credential },
                journal: completion.journal.clone(),
                accepted_at_ms: now_ms,
            }],
            qualification_expires_at_ms: completion_deadline(
                &completion.journal,
                qualified.expires_at_ms(),
            ),
        };
        if let Some((approval, expires)) = management {
            ensure!(
                approval.verified().credential_id() != registration.credential_id(),
                "new passkey cannot approve its own enrollment"
            );
            let (ledger, credential) = assertion_ledger(&approval)?;
            result.receipts.push(PasskeyReceipt {
                version: AUTH_VERSION,
                account: account.clone(),
                envelope_hash: ledger.envelope_hash.clone(),
                completion_hash,
                kind: PasskeyReceiptKind::AuthenticatorsManaged { credential },
                journal: completion.journal.clone(),
                accepted_at_ms: now_ms,
            });
            result.pending.push(approval.proof.pending.clone());
            result.ledger.push(ledger);
            result.qualification_expires_at_ms = result.qualification_expires_at_ms.min(expires);
        }
        Ok(result)
    }

    pub(crate) fn complete_passkey_management(
        &mut self,
        completion: &PasskeyManagementCompletion,
        context: &PasskeyContext,
        now_ms: u64,
    ) -> Result<PasskeyReceipt> {
        completion.intent.verify_signature()?;
        let account = &completion.intent.payload.account;
        let hash = completion_hash(completion)?;
        completion_pending(
            &self.connection,
            account,
            &completion.pending_id,
            context,
            PasskeyPurpose::ManageAuthenticators,
        )?;
        if let Some(receipt) = read_receipt(
            &self.connection,
            account,
            &completion.pending_id,
            Some(&hash),
        )? {
            return Ok(receipt);
        }
        ensure!(
            completion.journal.len() == 1,
            "authenticator revocation must contain exactly one account commit"
        );
        let proposal = account_proposal(&completion.journal[0])?;
        let approval = proposal
            .payload
            .authenticator_approval
            .as_ref()
            .context("authenticator management approval is missing")?;
        ensure!(
            approval.intent == completion.intent
                && approval
                    .assertion
                    .pending
                    .envelope_hash()
                    .map_err(anyhow::Error::msg)?
                    == completion.pending_id,
            "authenticator management changed its signed intent or pending"
        );
        let before = self
            .policy
            .verified()
            .account(account)
            .context("passkey account is missing")?;
        let mut expected = before.clone();
        expected.previous = self
            .policy
            .verified()
            .head(&PolicyDomain::Account(account.clone()))
            .cloned();
        expected.sequence = expected
            .sequence
            .checked_add(1)
            .context("account sequence overflow")?;
        expected.authenticators = proposal.payload.authenticators.clone();
        expected.authenticator_approval = Some(approval.clone());
        expected.device_enrollment = None;
        expected.root_acceptance = None;
        expected.recovery_approvals.clear();
        ensure!(
            expected.authenticators.len() < before.authenticators.len()
                && expected
                    .authenticators
                    .iter()
                    .all(|hash| before.authenticators.contains(hash)),
            "authenticator management can only revoke existing credentials"
        );
        ensure!(
            proposal.payload == expected,
            "authenticator management changed unrelated account policy"
        );
        let (response, expires) = self
            .management_approval(proposal, context, now_ms)?
            .context("authenticator management approval is missing")?;
        let update = self
            .policy
            .prepare_network_update(&completion.journal, now_ms)?;
        let (ledger, credential) = assertion_ledger(&response)?;
        let pending = response.proof.pending.clone();
        let prepared = PreparedPasskeyCompletion {
            pending: vec![pending],
            update,
            ledger: vec![ledger],
            receipts: vec![PasskeyReceipt {
                version: AUTH_VERSION,
                account: account.clone(),
                envelope_hash: completion.pending_id.clone(),
                completion_hash: hash,
                kind: PasskeyReceiptKind::AuthenticatorsManaged { credential },
                journal: completion.journal.clone(),
                accepted_at_ms: now_ms,
            }],
            qualification_expires_at_ms: completion_deadline(&completion.journal, expires),
        };
        self.commit_passkey_completion(prepared, now_ms)
    }

    pub(crate) fn verify_passkey_response(
        &self,
        account: &AccountId,
        pending_id: &ObjectHash,
        response_json: &str,
        context: &PasskeyContext,
        now_ms: u64,
    ) -> Result<VerifiedPasskeyResponse> {
        ensure!(
            response_json.len() <= 65_536,
            "passkey assertion response too large"
        );
        let response: AssertionResponse = serde_json::from_str(response_json)?;
        let pending = read_pending(&self.connection, account, pending_id)?;
        ensure!(
            pending.envelope().target == context.target
                && pending.relying_party() == &context.relying_party,
            "passkey assertion has the wrong local target or RP"
        );
        ensure!(
            read_receipt(&self.connection, account, pending_id, None)?.is_none(),
            "passkey request was already consumed"
        );
        require_current_heads(self.policy.verified(), pending.envelope())?;
        require_sql_heads(&self.connection, &pending.envelope().expected_heads)?;
        let policy = self.policy.verified();
        let account_document = policy
            .account(account)
            .context("passkey account is missing")?;
        let mut record = None;
        for hash in &account_document.authenticators {
            let Some(candidate) = policy.authenticator(hash) else {
                continue;
            };
            if candidate.payload.credential_id == response.raw_id {
                ensure!(record.is_none(), "ambiguous passkey credential");
                record = Some(candidate);
            }
        }
        let record = record.context("passkey credential is not active for this account")?;
        let authorized = policy.verify_authenticator(record, now_ms)?;
        let ledger = read_ledger(&self.connection, account, &response.raw_id)?;
        if let Some(ledger) = &ledger {
            ensure!(
                ledger.rp_id == authorized.rp_id(),
                "passkey ledger RP mismatch"
            );
        }
        let state = ledger_state(&ledger);
        let verified = super::webauthn::verify_device_approval(
            &pending,
            response_json,
            &authorized,
            state,
            now_ms,
        )?;
        let proof = PasskeyAssertionProof {
            pending,
            record: record.clone(),
            state,
            response,
        };
        Ok(VerifiedPasskeyResponse {
            proof,
            verified,
            ledger,
        })
    }

    pub(crate) fn complete_passkey_device(
        &mut self,
        completion: &PasskeyDeviceCompletion,
        context: &PasskeyContext,
        now_ms: u64,
    ) -> Result<PasskeyReceipt> {
        completion.intent.verify_signature()?;
        let account = &completion.intent.payload.account;
        let hash = completion_hash(completion)?;
        completion_pending(
            &self.connection,
            account,
            &completion.pending_id,
            context,
            PasskeyPurpose::EnrollDevice,
        )?;
        if let Some(receipt) = read_receipt(
            &self.connection,
            account,
            &completion.pending_id,
            Some(&hash),
        )? {
            return Ok(receipt);
        }
        let prepared = self.prepare_passkey_device_completion(completion, context, hash, now_ms)?;
        self.commit_passkey_completion(prepared, now_ms)
    }

    fn prepare_passkey_device_completion(
        &self,
        completion: &PasskeyDeviceCompletion,
        context: &PasskeyContext,
        completion_hash: ObjectHash,
        now_ms: u64,
    ) -> Result<PreparedPasskeyCompletion> {
        let account = &completion.intent.payload.account;
        let pending = completion_pending(
            &self.connection,
            account,
            &completion.pending_id,
            context,
            PasskeyPurpose::EnrollDevice,
        )?;
        ensure!(
            completion.journal.len() == 1,
            "passkey device enrollment must contain exactly one account commit"
        );
        let JournalEntry::AccountCommit(committed) = &completion.journal[0] else {
            bail!("passkey device enrollment requires an authority-committed policy");
        };
        let proof = committed
            .proposal
            .payload
            .device_enrollment
            .as_ref()
            .context("passkey device enrollment proof is missing")?;
        ensure!(
            proof.intent == completion.intent && proof.assertion.pending == pending,
            "passkey device completion changed its signed intent or trusted pending"
        );
        let response = self.verify_passkey_response(
            account,
            &completion.pending_id,
            &serde_json::to_string(&proof.assertion.response)?,
            context,
            now_ms,
        )?;
        ensure!(
            response.proof() == &proof.assertion,
            "passkey device completion changed its trusted credential state"
        );
        let qualified = verify_passkey_device_enrollment(self.policy.verified(), proof, now_ms)?;
        ensure!(
            qualified.verified_assertion().previous_state() == ledger_state(&response.ledger),
            "passkey enrollment used another credential state"
        );
        let update = self
            .policy
            .prepare_network_update(&completion.journal, now_ms)?;
        let (ledger, credential) = assertion_ledger(&response)?;
        let receipt = PasskeyReceipt {
            version: AUTH_VERSION,
            account: account.clone(),
            envelope_hash: completion.pending_id.clone(),
            completion_hash,
            kind: PasskeyReceiptKind::DeviceEnrolled {
                device: qualified.certificate().payload.device.clone(),
                credential,
            },
            journal: completion.journal.clone(),
            accepted_at_ms: now_ms,
        };
        Ok(PreparedPasskeyCompletion {
            pending: vec![pending],
            update,
            ledger: vec![ledger],
            receipts: vec![receipt],
            qualification_expires_at_ms: completion_deadline(
                &completion.journal,
                qualified.expires_at_ms(),
            ),
        })
    }

    pub(crate) fn commit_passkey_completion(
        &mut self,
        prepared: PreparedPasskeyCompletion,
        now_ms: u64,
    ) -> Result<PasskeyReceipt> {
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let primary = prepared
            .receipts
            .first()
            .context("passkey completion has no receipt")?;
        let account = &primary.account;
        let id = &primary.envelope_hash;
        if let Some(receipt) = read_receipt(&tx, account, id, Some(&primary.completion_hash))? {
            tx.commit()?;
            return Ok(receipt);
        }
        ensure!(
            now_ms < prepared.qualification_expires_at_ms,
            "passkey qualification expired during verification"
        );
        for expected in &prepared.pending {
            let id = expected.envelope_hash().map_err(anyhow::Error::msg)?;
            let pending = read_pending(&tx, account, &id)?;
            ensure!(
                pending == *expected,
                "passkey pending changed during verification"
            );
            ensure!(
                pending.envelope().not_before_ms <= now_ms
                    && now_ms < pending.envelope().expires_at_ms,
                "passkey completion expired during verification"
            );
            ensure!(
                read_receipt(&tx, account, &id, None)?.is_none(),
                "passkey approval was already consumed"
            );
        }
        let state = journal::commit_in(&tx, &prepared.update)?;
        for mutation in &prepared.ledger {
            write_ledger_in(&tx, account, mutation)?;
        }
        let mut receipts = prepared.receipts;
        for receipt in &mut receipts {
            receipt.accepted_at_ms = now_ms;
            tx.execute(
                "INSERT INTO passkey_receipts(account_id,envelope_hash,completion_hash,receipt,accepted_at_ms) VALUES(?,?,?,?,?)",
                params![receipt.account.as_str(), receipt.envelope_hash.as_str(), receipt.completion_hash.as_str(), canonical(&receipt)?, now_ms],
            )?;
        }
        tx.commit()?;
        self.policy = state;
        Ok(receipts.remove(0))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
    use p256::ecdsa::{Signature, SigningKey, signature::Signer};
    use std::path::Path;
    use vibeface_secure::{
        Identity,
        authorization::{
            Actor, AuthorityBinding, Committed, DeviceCertificate, DeviceUse, FreshnessReceipt,
            PasskeyClientEnrollment, PolicyCommit, PolicyPin, PublicKey, RecoveryPolicy,
            enrollment::PasskeyEnrollmentProof,
        },
    };

    const NOW: u64 = 1000;

    fn key(byte: u8) -> Identity {
        Identity::from_secret_hex(&hex::encode([byte; 32])).unwrap()
    }

    struct Fixture {
        store: Store,
        root: Identity,
        authority: Identity,
        requester: Identity,
        signing: SigningKey,
        account: AccountId,
        context: PasskeyContext,
    }

    impl Fixture {
        fn new() -> Self {
            let root = key(1);
            let authority = key(2);
            let requester = key(4);
            let public = PublicKey::of(&root);
            let account = AccountId::from_root(&public);
            let document = AccountDocument {
                version: AUTH_VERSION,
                account: account.clone(),
                genesis_root: public.clone(),
                current_root: public.clone(),
                root_generation: 0,
                previous: None,
                authority_epoch: 0,
                sequence: 0,
                authority: AuthorityBinding::from_key(PublicKey::of(&authority)),
                recovery: RecoveryPolicy {
                    generation: 0,
                    keys: vec![PublicKey::of(&key(3))],
                    threshold: 1,
                },
                enrollment: vec![],
                devices: vec![],
                revoked_devices: vec![],
                authenticators: vec![],
                authenticator_approval: None,
                device_enrollment: None,
                root_acceptance: None,
                recovery_approvals: vec![],
            };
            let genesis = Signed::sign(document, &root).unwrap();
            let mut store = Store::initialize(Path::new(":memory:")).unwrap();
            let initial = store
                .policy
                .prepare_bootstrap(
                    &[PolicyPin {
                        domain: PolicyDomain::Account(account.clone()),
                        genesis_root: public,
                        minimum_head: None,
                    }],
                    &[JournalEntry::AccountGenesis(genesis)],
                    NOW,
                )
                .unwrap();
            let tx = store.connection.transaction().unwrap();
            let state = journal::commit_in(&tx, &initial).unwrap();
            tx.commit().unwrap();
            store.policy = state;
            let context = PasskeyContext {
                target: DeviceId::from_key(&PublicKey::of(&authority)),
                relying_party: RelyingParty::new("localhost", "http://localhost:4310").unwrap(),
            };
            let mut f = Self {
                store,
                root,
                authority,
                requester,
                account,
                context,
                signing: SigningKey::from_slice(&[9; 32]).unwrap(),
            };
            let mut certificate = f.certificate(&f.requester, Nonce::from_bytes([1; 32]));
            certificate.prove_possession(&f.requester).unwrap();
            let certificate = Signed::sign(certificate, &f.root).unwrap();
            let mut document = f.next_document();
            document.devices.push(certificate.hash().unwrap());
            let committed = f.account_commit(document, &f.root, 1);
            f.apply(&[JournalEntry::Device(certificate), committed]);
            f
        }

        fn head(&self) -> PolicyStamp {
            self.store
                .policy
                .verified()
                .head(&PolicyDomain::Account(self.account.clone()))
                .unwrap()
                .clone()
        }

        fn next_document(&self) -> AccountDocument {
            let mut document = self
                .store
                .policy
                .verified()
                .account(&self.account)
                .unwrap()
                .clone();
            document.previous = Some(self.head());
            document.sequence += 1;
            document.device_enrollment = None;
            document.authenticator_approval = None;
            document
        }

        fn certificate(&self, device: &Identity, nonce: Nonce) -> DeviceCertificate {
            DeviceCertificate {
                version: AUTH_VERSION,
                owner: PolicyDomain::Account(self.account.clone()),
                subject: Actor::Account(self.account.clone()),
                device: DeviceId::from_key(&PublicKey::of(device)),
                public_key: PublicKey::of(device),
                uses: vec![DeviceUse::Noise, DeviceUse::AccountInvocation],
                possession: None,
                enrollment_grant: None,
                issued_under: self.head(),
                nonce,
                not_before_ms: NOW,
                expires_at_ms: NOW + 500_000,
            }
        }

        fn account_commit(
            &self,
            document: AccountDocument,
            signer: &Identity,
            nonce: u8,
        ) -> JournalEntry {
            let proposal = Signed::sign(document, signer).unwrap();
            let previous = self.head();
            let commit = Signed::sign(
                PolicyCommit {
                    version: AUTH_VERSION,
                    previous: previous.clone(),
                    proposal_hash: proposal.hash().unwrap(),
                    nonce: Nonce::from_bytes([nonce; 32]),
                    dependencies: vec![previous.clone()],
                    issued_at_ms: NOW,
                    expires_at_ms: NOW + 100_000,
                    accepted_at_ms: NOW,
                },
                &self.authority,
            )
            .unwrap();
            let receipt = Signed::sign(
                FreshnessReceipt {
                    version: AUTH_VERSION,
                    current: previous,
                    challenge_hash: commit.challenge_hash().unwrap(),
                    issued_at_ms: NOW,
                    expires_at_ms: NOW + 100_000,
                },
                &self.authority,
            )
            .unwrap();
            JournalEntry::AccountCommit(Committed {
                proposal,
                commit,
                receipts: vec![receipt],
            })
        }

        fn apply(&mut self, entries: &[JournalEntry]) {
            let prepared = self
                .store
                .policy
                .prepare_network_update(entries, NOW)
                .unwrap();
            let tx = self.store.connection.transaction().unwrap();
            let state = journal::commit_in(&tx, &prepared).unwrap();
            tx.commit().unwrap();
            self.store.policy = state;
        }

        fn registration(&mut self) -> PasskeyRegistrationCompletion {
            let intent = Signed::sign(
                AuthenticatorEnrollmentIntent {
                    account: self.account.clone(),
                    requesting_device_public_key: PublicKey::of(&self.requester),
                    target: self.context.target.clone(),
                    credential_not_before_ms: NOW,
                    credential_expires_at_ms: NOW + 500_000,
                    expected_heads: vec![self.head()],
                },
                &self.root,
            )
            .unwrap();
            let prepared = self
                .store
                .prepare_passkey_registration(&intent, &self.context, NOW)
                .unwrap();
            let PasskeyPreparation::Pending {
                pending_id,
                pending,
                ..
            } = prepared
            else {
                panic!("pending expected")
            };
            let credential = URL_SAFE_NO_PAD.encode([42; 32]);
            let point = self.signing.verifying_key().to_encoded_point(false);
            let mut cose = vec![0xa5, 0x01, 0x02, 0x03, 0x26, 0x20, 0x01, 0x21, 0x58, 0x20];
            cose.extend_from_slice(point.x().unwrap());
            cose.extend_from_slice(&[0x22, 0x58, 0x20]);
            cose.extend_from_slice(point.y().unwrap());
            let mut data = Sha256::digest(b"localhost").to_vec();
            data.push(0x45);
            data.extend_from_slice(&1u32.to_be_bytes());
            data.extend_from_slice(&[0; 16]);
            data.extend_from_slice(&32u16.to_be_bytes());
            data.extend_from_slice(&[42; 32]);
            data.extend_from_slice(&cose);
            let mut attestation = vec![
                0xa3,
                0x63,
                b'f',
                b'm',
                b't',
                0x64,
                b'n',
                b'o',
                b'n',
                b'e',
                0x67,
                b'a',
                b't',
                b't',
                b'S',
                b't',
                b'm',
                b't',
                0xa0,
                0x68,
                b'a',
                b'u',
                b't',
                b'h',
                b'D',
                b'a',
                b't',
                b'a',
                0x58,
                data.len() as u8,
            ];
            attestation.extend_from_slice(&data);
            let options = pending.creation_options().unwrap();
            let client = serde_json::to_vec(&serde_json::json!({
                "type":"webauthn.create", "challenge":options["publicKey"]["challenge"],
                "origin":"http://localhost:4310", "crossOrigin":false,
            }))
            .unwrap();
            let response: RegistrationResponse = serde_json::from_value(serde_json::json!({
                "id":credential, "rawId":credential, "type":"public-key",
                "response":{"clientDataJSON":URL_SAFE_NO_PAD.encode(client),
                    "attestationObject":URL_SAFE_NO_PAD.encode(attestation)},
                "clientExtensionResults":{},
            }))
            .unwrap();
            let registration = verify_passkey_registration(&pending, &response, NOW).unwrap();
            let record = Signed::sign(
                AuthenticatorRecord {
                    version: AUTH_VERSION,
                    account: self.account.clone(),
                    credential_id: registration.credential_id().to_owned(),
                    cose_public_key: registration.cose_public_key().to_vec(),
                    rp_id: "localhost".into(),
                    origins: vec!["http://localhost:4310".into()],
                    user_handle: pending.user_handle().to_owned(),
                    backup_eligible: false,
                    client_enrollment: PasskeyClientEnrollment::AccountClient,
                    issued_under: self.head(),
                    nonce: pending.envelope().nonce.clone(),
                    enrollment_hash: pending_id.clone(),
                    not_before_ms: NOW,
                    expires_at_ms: NOW + 500_000,
                },
                &self.root,
            )
            .unwrap();
            let mut document = self.next_document();
            document.authenticators.push(record.hash().unwrap());
            let commit = self.account_commit(document, &self.root, 2);
            PasskeyRegistrationCompletion {
                intent,
                pending_id,
                response,
                journal: vec![JournalEntry::Authenticator(record), commit],
            }
        }

        fn enroll_device(&mut self, device: &Identity, counter: u32) -> PasskeyDeviceCompletion {
            let intent = Signed::sign(
                DeviceEnrollmentIntent {
                    account: self.account.clone(),
                    new_device_public_key: PublicKey::of(device),
                    requested_rights: vec![],
                    certificate_not_before_ms: NOW,
                    certificate_expires_at_ms: NOW + 500_000,
                    target: self.context.target.clone(),
                    expected_heads: vec![self.head()],
                },
                device,
            )
            .unwrap();
            let preparation = self
                .store
                .prepare_passkey_device(&intent, &self.context, NOW)
                .unwrap();
            let PasskeyPreparation::Pending {
                pending_id,
                pending,
                options,
            } = preparation
            else {
                panic!("pending expected")
            };
            assert_eq!(
                options["publicKey"]["allowCredentials"],
                serde_json::json!([])
            );
            let client = serde_json::to_vec(&serde_json::json!({
                "type":"webauthn.get", "challenge":options["publicKey"]["challenge"],
                "origin":"http://localhost:4310", "crossOrigin":false,
            }))
            .unwrap();
            let mut auth = Sha256::digest(b"localhost").to_vec();
            auth.push(0x05);
            auth.extend_from_slice(&counter.to_be_bytes());
            let mut message = auth.clone();
            message.extend_from_slice(&Sha256::digest(&client));
            let signature: Signature = self.signing.sign(&message);
            let credential = URL_SAFE_NO_PAD.encode([42; 32]);
            let response = serde_json::json!({
                "id":credential,"rawId":credential,"type":"public-key",
                "response":{"clientDataJSON":URL_SAFE_NO_PAD.encode(client),
                    "authenticatorData":URL_SAFE_NO_PAD.encode(auth),
                    "signature":URL_SAFE_NO_PAD.encode(signature.to_der().as_bytes()),
                    "userHandle":pending.user_handle()},"clientExtensionResults":{},
            });
            let verified = self
                .store
                .verify_passkey_response(
                    &self.account,
                    &pending_id,
                    &response.to_string(),
                    &self.context,
                    NOW,
                )
                .unwrap();
            let mut certificate = self.certificate(device, pending.envelope().nonce.clone());
            certificate.prove_possession(device).unwrap();
            let certificate = Signed::sign(certificate, device).unwrap();
            let proof = PasskeyEnrollmentProof {
                intent: intent.clone(),
                certificate: certificate.clone(),
                assertion: verified.proof().clone(),
            };
            let mut document = self.next_document();
            document.devices.push(certificate.hash().unwrap());
            document.devices.sort();
            document.device_enrollment = Some(proof);
            let committed = self.account_commit(document, device, 3);
            PasskeyDeviceCompletion {
                intent,
                pending_id,
                journal: vec![committed],
            }
        }

        fn counts(&self) -> (u64, u64, u64) {
            self.store.connection.query_row("SELECT (SELECT count(*) FROM policy_journal),
                (SELECT count(*) FROM passkey_authenticators),(SELECT count(*) FROM passkey_receipts)",
                [], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?))).unwrap()
        }
    }

    #[test]
    fn real_store_registration_and_device_enrollment_replay_the_original_receipts() {
        let mut f = Fixture::new();
        let registration = f.registration();
        let original_pending =
            read_pending(&f.store.connection, &f.account, &registration.pending_id).unwrap();
        let retried = f
            .store
            .prepare_passkey_registration(&registration.intent, &f.context, NOW + 1)
            .unwrap();
        let PasskeyPreparation::Pending { pending, .. } = retried else {
            panic!("pending expected")
        };
        assert_eq!(pending, original_pending);
        let receipt = f
            .store
            .complete_passkey_registration(&registration, &f.context, NOW)
            .unwrap();
        assert!(matches!(
            receipt.kind,
            PasskeyReceiptKind::AuthenticatorRegistered { .. }
        ));
        let counts = f.counts();
        let outsider = Signed::sign(registration.intent.payload.clone(), &key(8)).unwrap();
        assert!(
            f.store
                .prepare_passkey_registration(&outsider, &f.context, NOW)
                .is_err()
        );
        assert!(
            f.store
                .reject_passkey_policy_bypass(&registration.journal, &f.context.target)
                .is_ok()
        );
        assert_eq!(
            f.store
                .complete_passkey_registration(&registration, &f.context, NOW + 600_000)
                .unwrap(),
            receipt
        );
        let retried = f
            .store
            .prepare_passkey_registration(&registration.intent, &f.context, NOW + 600_000)
            .unwrap();
        assert!(
            matches!(retried, PasskeyPreparation::Completed { receipt: old } if old == receipt)
        );
        assert_eq!(f.counts(), counts);
        let mut altered = registration.clone();
        altered.response.client_extension_results = serde_json::json!({"different":true});
        assert!(
            f.store
                .complete_passkey_registration(&altered, &f.context, NOW)
                .is_err()
        );
        assert_eq!(f.counts(), counts);
        let device = key(5);
        let completion = f.enroll_device(&device, 2);
        assert!(
            f.store
                .reject_passkey_policy_bypass(&completion.journal, &f.context.target)
                .is_err()
        );
        let receipt = f
            .store
            .complete_passkey_device(&completion, &f.context, NOW)
            .unwrap();
        assert!(matches!(
            receipt.kind,
            PasskeyReceiptKind::DeviceEnrolled { .. }
        ));
        assert_eq!(
            read_ledger(
                &f.store.connection,
                &f.account,
                &URL_SAFE_NO_PAD.encode([42; 32])
            )
            .unwrap()
            .unwrap()
            .state
            .sign_count,
            2
        );
        f.store.policy = PolicyState::load(&f.store.connection).unwrap();
        assert_eq!(
            f.store
                .complete_passkey_device(&completion, &f.context, NOW + 600_000)
                .unwrap(),
            receipt
        );
        let counts = f.counts();
        assert_eq!(counts.2, 2);
        let record = f.store.policy.verified().account(&f.account).unwrap();
        assert_eq!(record.devices.len(), 2);
        assert!(record.enrollment.is_empty());
    }

    #[test]
    fn expiry_does_not_regenerate_an_intent_and_a_failed_receipt_rolls_back_every_write() {
        let mut f = Fixture::new();
        let registration = f.registration();
        let before = f.counts();
        assert!(
            f.store
                .reject_passkey_policy_bypass(&registration.journal, &f.context.target)
                .is_err()
        );
        let original =
            read_pending(&f.store.connection, &f.account, &registration.pending_id).unwrap();
        assert!(
            f.store
                .prepare_passkey_registration(
                    &registration.intent,
                    &f.context,
                    original.envelope().expires_at_ms
                )
                .is_err()
        );
        assert_eq!(
            read_pending(&f.store.connection, &f.account, &registration.pending_id).unwrap(),
            original
        );
        f.store.connection.execute_batch("CREATE TEMP TRIGGER reject_passkey_receipt BEFORE INSERT ON passkey_receipts BEGIN SELECT RAISE(ABORT,'receipt rejected'); END").unwrap();
        let head = f.head();
        assert!(
            f.store
                .complete_passkey_registration(&registration, &f.context, NOW)
                .is_err()
        );
        assert_eq!(f.counts(), before);
        assert_eq!(f.head(), head);
        assert_eq!(
            PolicyState::load(&f.store.connection)
                .unwrap()
                .verified()
                .head(&head.domain),
            Some(&head)
        );
        f.store
            .connection
            .execute_batch("DROP TRIGGER reject_passkey_receipt")
            .unwrap();
        f.store
            .complete_passkey_registration(&registration, &f.context, NOW)
            .unwrap();
        let device = key(5);
        let completion = f.enroll_device(&device, 2);
        let before = f.counts();
        let head = f.head();
        f.store.connection.execute_batch("CREATE TEMP TRIGGER reject_passkey_receipt BEFORE INSERT ON passkey_receipts BEGIN SELECT RAISE(ABORT,'receipt rejected'); END").unwrap();
        assert!(
            f.store
                .complete_passkey_device(&completion, &f.context, NOW)
                .is_err()
        );
        assert_eq!(f.counts(), before);
        assert_eq!(f.head(), head);
        assert_eq!(
            read_ledger(
                &f.store.connection,
                &f.account,
                &URL_SAFE_NO_PAD.encode([42; 32])
            )
            .unwrap()
            .unwrap()
            .state
            .sign_count,
            1
        );
    }

    #[test]
    fn completion_uses_the_current_counter_and_rechecks_policy_and_ledger_at_commit() {
        let mut f = Fixture::new();
        let registration = f.registration();
        f.store
            .complete_passkey_registration(&registration, &f.context, NOW)
            .unwrap();
        let device = key(5);
        let completion = f.enroll_device(&device, 2);
        let hash = completion_hash(&completion).unwrap();
        let mut forged_document = account_proposal(&completion.journal[0])
            .unwrap()
            .payload
            .clone();
        forged_document
            .device_enrollment
            .as_mut()
            .unwrap()
            .assertion
            .state
            .sign_count = 0;
        let mut forged = completion.clone();
        forged.journal = vec![f.account_commit(forged_document, &device, 5)];
        assert!(
            f.store
                .complete_passkey_device(&forged, &f.context, NOW)
                .is_err()
        );
        let prepared = f
            .store
            .prepare_passkey_device_completion(&completion, &f.context, hash.clone(), NOW)
            .unwrap();
        let counts = f.counts();
        assert!(
            f.store
                .commit_passkey_completion(prepared, NOW + 100_000)
                .is_err()
        );
        assert_eq!(f.counts(), counts);
        let prepared = f
            .store
            .prepare_passkey_device_completion(&completion, &f.context, hash.clone(), NOW)
            .unwrap();
        let counts = f.counts();
        f.store
            .connection
            .execute("UPDATE passkey_authenticators SET sign_count=3", [])
            .unwrap();
        assert!(f.store.commit_passkey_completion(prepared, NOW).is_err());
        assert_eq!(f.counts(), counts);
        assert!(
            f.store
                .complete_passkey_device(&completion, &f.context, NOW)
                .is_err()
        );
        f.store
            .connection
            .execute("UPDATE passkey_authenticators SET sign_count=1", [])
            .unwrap();
        let prepared = f
            .store
            .prepare_passkey_device_completion(&completion, &f.context, hash.clone(), NOW)
            .unwrap();
        let mut revoked = f.next_document();
        revoked.authenticators.clear();
        f.apply(&[f.account_commit(revoked, &f.root, 4)]);
        let counts = f.counts();
        assert!(f.store.commit_passkey_completion(prepared, NOW).is_err());
        assert_eq!(f.counts(), counts);
        assert!(
            f.store
                .complete_passkey_device(&completion, &f.context, NOW)
                .is_err()
        );
    }
}
