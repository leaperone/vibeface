pub(crate) mod target_cache;

use super::journal::{AcceptedJournalCursor, AcceptedJournalSnapshot, PolicyState};
use crate::{crypto::LocalIdentity, store::Store};
use anyhow::{Context, Result, ensure};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use vibeface_secure::{
    Identity,
    authorization::{
        AUTH_VERSION, AuthorityBinding, DeviceId, Nonce, ObjectHash, PolicyDomain, PolicyStamp,
        PublicKey, Signed, VerifiedPolicySet,
        bootstrap::{self, NodeClaim, recovery::stream::*},
    },
    signature_verification::ScopedSignatureVerification,
};

const MAX_CARRIER_BYTES: usize = 65_536;
const MAX_STORED_CONTEXT_BYTES: usize = MAX_CARRIER_BYTES * 2;

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RecoveryReadRecord {
    request: RecoveryReadRequest,
    snapshot: Signed<RecoveryContextSnapshot>,
    snapshot_salt: Nonce,
    journal_snapshot: AcceptedJournalSnapshot,
    position: RecoveryReadPosition,
    witness: Option<Signed<RecoveryContextWitness>>,
    source_approval: Option<Signed<RecoverySourceProposalApproval>>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RecoveryReadPosition {
    cursor: Option<ObjectHash>,
    journal_cursor: Option<AcceptedJournalCursor>,
    page_count: u64,
    entry_count: u64,
    previous_page_hash: ObjectHash,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CachedRecoveryPage {
    request_cursor: Option<ObjectHash>,
    page: Signed<RecoveryContextPage>,
}

pub(crate) struct CompletedRecoveryRead {
    pub(crate) request: RecoveryReadRequest,
    pub(crate) snapshot: Signed<RecoveryContextSnapshot>,
    pub(crate) witness: Signed<RecoveryContextWitness>,
    pub(crate) source_approval: Option<Signed<RecoverySourceProposalApproval>>,
    journal_snapshot: AcceptedJournalSnapshot,
    last_page: Option<Signed<RecoveryContextPage>>,
}

impl CompletedRecoveryRead {
    pub(crate) fn accepted_prefix_commitment_in(
        &self,
        connection: &Connection,
        policy: &PolicyState,
    ) -> Result<ObjectHash> {
        policy.accepted_journal_page(
            connection,
            &self.journal_snapshot,
            None,
            &Default::default(),
        )?;
        Ok(ObjectHash::body(
            &policy.accepted_journal_snapshot(connection)?,
        )?)
    }

    pub(crate) fn record_current_usage(
        &self,
        state: &VerifiedPolicySet,
        usage: &mut RecoveryProofUsage,
    ) -> Result<()> {
        self.record_reference_usage(usage)?;
        usage.record_current_source(&self.request, state)?;
        if let Some(page) = &self.last_page {
            usage.record_page_binding(page)?;
        }
        Ok(())
    }

    pub(crate) fn record_reference_usage(&self, usage: &mut RecoveryProofUsage) -> Result<()> {
        usage.record_request(&self.request)?;
        usage.record_snapshot(&self.snapshot)?;
        usage.record_witness(&self.witness)?;
        Ok(())
    }
}

fn reference_record_in(
    connection: &Connection,
    identity: &LocalIdentity,
    principal: &str,
    reference: &RecoveryContextReference,
) -> Result<RecoveryReadRecord> {
    let record: RecoveryReadRecord = serde_json::from_str(
        &metadata(connection, &context_key(&reference.snapshot_id))?
            .context("recovery snapshot is missing")?,
    )?;
    let witness = record
        .witness
        .as_ref()
        .context("recovery import is not complete")?;
    let local = PublicKey::parse(&identity.public_key_hex()?)?;
    ensure!(
        record.request.new_device_public_key().as_str() == principal
            && record.snapshot.issuer == local
            && witness.issuer == local
            && record.snapshot.payload.version == AUTH_VERSION
            && witness.payload.version == AUTH_VERSION
            && record.snapshot.payload.kind == record.request.kind()
            && witness.payload.kind == record.request.kind()
            && record.snapshot.payload.snapshot_id == reference.snapshot_id
            && record.snapshot.payload.request_hash == reference.request_hash
            && record.request.request_hash()? == reference.request_hash
            && record.snapshot.hash()? == reference.snapshot_hash
            && witness.hash()? == reference.witness_hash
            && witness.payload.snapshot_id == reference.snapshot_id
            && witness.payload.request_hash == reference.request_hash
            && witness.payload.target_heads == record.snapshot.payload.target_heads
            && witness.payload.claim_hash == record.snapshot.payload.claim.hash()?
            && record.snapshot.payload.saved_pins_hash == record.request.saved_pins_hash()?
            && record.snapshot.payload.expires_at_ms == record.request.expires_at_ms()
            && witness.payload.expires_at_ms == record.request.expires_at_ms()
            && record.request.issued_at_ms() <= record.snapshot.payload.issued_at_ms
            && record.snapshot.payload.issued_at_ms <= witness.payload.issued_at_ms
            && witness.payload.issued_at_ms < witness.payload.expires_at_ms
            && witness.payload.source_node_public_key == local
            && record.snapshot.payload.source_node_public_key == local,
        "recovery completion reference or Noise device changed"
    );
    record.snapshot.verify_signature()?;
    witness.verify_signature()?;
    Ok(record)
}

pub(crate) fn recovery_read_reference_in(
    connection: &Connection,
    identity: &LocalIdentity,
    principal: &str,
    reference: &RecoveryContextReference,
) -> Result<CompletedRecoveryRead> {
    let record = reference_record_in(connection, identity, principal, reference)?;
    Ok(CompletedRecoveryRead {
        request: record.request,
        snapshot: record.snapshot,
        witness: record.witness.context("recovery import is not complete")?,
        source_approval: record.source_approval,
        journal_snapshot: record.journal_snapshot,
        last_page: None,
    })
}

pub(crate) fn completed_recovery_read_in(
    connection: &Connection,
    policy: &PolicyState,
    identity: &LocalIdentity,
    principal: &str,
    reference: &RecoveryContextReference,
    now: u64,
) -> Result<CompletedRecoveryRead> {
    let record = reference_record_in(connection, identity, principal, reference)?;
    let cached = verify_record(
        connection,
        policy,
        &record,
        &PublicKey::parse(&identity.public_key_hex()?)?,
        principal,
        now,
    )?;
    Ok(CompletedRecoveryRead {
        request: record.request,
        snapshot: record.snapshot,
        witness: record.witness.context("recovery import is not complete")?,
        source_approval: record.source_approval,
        journal_snapshot: record.journal_snapshot,
        last_page: cached.map(|cached| cached.page),
    })
}

pub(crate) fn store_source_proposal_approval_in(
    connection: &Connection,
    context: &CompletedRecoveryRead,
    approval: &Signed<RecoverySourceProposalApproval>,
    accepted_prefix_commitment: &ObjectHash,
) -> Result<()> {
    let key = context_key(&context.snapshot.payload.snapshot_id);
    let encoded = metadata(connection, &key)?.context("source recovery record is missing")?;
    let mut record: RecoveryReadRecord = serde_json::from_str(&encoded)?;
    ensure!(
        record.request == context.request
            && record.snapshot == context.snapshot
            && record.witness.as_ref() == Some(&context.witness)
            && approval.issuer == record.snapshot.issuer
            && approval.payload.context.snapshot_id == record.snapshot.payload.snapshot_id
            && approval.payload.context.request_hash == record.request.request_hash()?
            && approval.payload.context.snapshot_hash == record.snapshot.hash()?
            && approval.payload.context.witness_hash == context.witness.hash()?
            && &approval.payload.accepted_prefix_commitment == accepted_prefix_commitment
            && approval.payload.expected_heads == record.snapshot.payload.target_heads,
        "source proposal approval changed its original accepted read"
    );
    approval.verify_signature()?;
    if let Some(previous) = &record.source_approval {
        ensure!(
            previous == approval,
            "source recovery already approved another proposal"
        );
        return Ok(());
    }
    record.source_approval = Some(approval.clone());
    ensure!(
        connection.execute(
            "UPDATE identity_meta SET value=? WHERE key=? AND value=?",
            params![encode_record(&record)?, key, encoded],
        )? == 1,
        "source recovery record changed concurrently"
    );
    Ok(())
}

fn context_key(id: &Nonce) -> String {
    format!("recovery-read:{id}")
}

fn page_key(id: &Nonce) -> String {
    format!("recovery-read-page:{id}")
}

fn request_key(request: &RecoveryReadRequest) -> String {
    let kind = match request.kind() {
        RecoveryContextKind::Account => "account",
        RecoveryContextKind::CrossNode => "cross-node",
    };
    format!("recovery-read-request:{kind}:{}", request.nonce())
}

fn metadata(connection: &Connection, key: &str) -> Result<Option<String>> {
    let length: Option<usize> = connection
        .query_row(
            "SELECT length(CAST(value AS BLOB)) FROM identity_meta WHERE key=?",
            [key],
            |row| row.get(0),
        )
        .optional()?;
    ensure!(
        length.is_none_or(|length| length <= MAX_STORED_CONTEXT_BYTES),
        "stored recovery context is too large"
    );
    Ok(connection
        .query_row(
            "SELECT value FROM identity_meta WHERE key=?",
            [key],
            |row| row.get(0),
        )
        .optional()?)
}

fn encode_record<T: Serialize>(record: &T) -> Result<String> {
    let encoded = serde_json::to_string(record)?;
    ensure!(
        encoded.len() <= MAX_STORED_CONTEXT_BYTES,
        "stored recovery context is too large"
    );
    Ok(encoded)
}

fn bounded_value(value: Value) -> Result<Value> {
    ensure!(
        serde_json::to_vec(&value)?.len() <= MAX_CARRIER_BYTES,
        "recovery carrier exceeds 64 KiB"
    );
    Ok(value)
}

fn signer(identity: &LocalIdentity) -> Result<Identity> {
    Identity::from_secret_hex(&identity.secret_key_hex).map_err(anyhow::Error::msg)
}

fn snapshot_id(
    request_hash: &ObjectHash,
    snapshot: &AcceptedJournalSnapshot,
    salt: &Nonce,
) -> Result<Nonce> {
    let hash = ObjectHash::body(&("recovery-read-snapshot-v2", request_hash, snapshot, salt))?;
    Ok(Nonce::parse(
        hash.as_str()
            .strip_prefix("h2_")
            .context("invalid recovery snapshot hash")?,
    )?)
}

fn cursor_hash(
    snapshot: &Signed<RecoveryContextSnapshot>,
    salt: &Nonce,
    position: &RecoveryReadPosition,
    page_previous_hash: &ObjectHash,
) -> Result<ObjectHash> {
    Ok(ObjectHash::body(&(
        "recovery-read-cursor-v2",
        snapshot.hash()?,
        salt,
        &position.journal_cursor,
        position.page_count,
        position.entry_count,
        page_previous_hash,
    ))?)
}

fn check_heads_in(connection: &Connection, heads: &[PolicyStamp]) -> Result<()> {
    for head in heads {
        let (kind, id) = match &head.domain {
            PolicyDomain::Account(id) => ("account", id.as_str()),
            PolicyDomain::Space(id) => ("space", id.as_str()),
        };
        let actual: Option<(u64, u64, String)> = connection
            .query_row(
                "SELECT authority_epoch,sequence,head_hash FROM current_policy_heads WHERE domain_kind=? AND domain_id=?",
                params![kind, id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()?;
        ensure!(
            actual == Some((head.authority_epoch, head.sequence, head.head.to_string())),
            "recovery policy-changed"
        );
    }
    Ok(())
}

fn verify_request(
    policy: &PolicyState,
    request: &RecoveryReadRequest,
    local: &PublicKey,
    principal: &str,
    now: u64,
) -> Result<Vec<PolicyStamp>> {
    ensure!(
        request.new_device_public_key().as_str() == principal,
        "recovery belongs to another Noise device"
    );
    request.verify(policy.verified(), local, now)?;
    policy
        .verified()
        .verify_transport_device(&DeviceId::from_key(local), now)?;
    let authority = AuthorityBinding::from_key(local.clone());
    ensure!(
        policy
            .verified()
            .account(request.account())
            .is_some_and(|account| account.authority == authority)
            && policy
                .verified()
                .space(request.space())
                .is_some_and(|space| space.authority == authority),
        "recovery source is not the current personal authority"
    );
    request
        .saved_pins()
        .iter()
        .map(|pin| {
            ensure!(
                policy.pins().iter().any(|known| {
                    known.domain == pin.domain && known.genesis_root == pin.genesis_root
                }),
                "recovery saved genesis differs from the installed root"
            );
            policy
                .verified()
                .head(&pin.domain)
                .cloned()
                .context("recovery policy head is missing")
        })
        .collect()
}

fn verify_record(
    connection: &Connection,
    policy: &PolicyState,
    record: &RecoveryReadRecord,
    local: &PublicKey,
    principal: &str,
    now: u64,
) -> Result<Option<CachedRecoveryPage>> {
    let heads = verify_request(policy, &record.request, local, principal, now)?;
    let snapshot = &record.snapshot.payload;
    ensure!(
        record.snapshot.issuer == *local
            && snapshot.version == AUTH_VERSION
            && snapshot.kind == record.request.kind()
            && snapshot.request_hash == record.request.request_hash()?
            && snapshot.saved_pins_hash == record.request.saved_pins_hash()?
            && snapshot.source_node_public_key == *local
            && snapshot.target_heads == heads
            && snapshot.snapshot_id
                == snapshot_id(
                    &snapshot.request_hash,
                    &record.journal_snapshot,
                    &record.snapshot_salt,
                )?
            && record.request.issued_at_ms() <= snapshot.issued_at_ms
            && snapshot.issued_at_ms <= now
            && snapshot.expires_at_ms == record.request.expires_at_ms()
            && now < snapshot.expires_at_ms,
        "recovery snapshot or policy-changed"
    );
    record.snapshot.verify_signature()?;
    check_heads_in(connection, &heads)?;
    let cached = metadata(connection, &page_key(&snapshot.snapshot_id))?
        .map(|encoded| serde_json::from_str::<CachedRecoveryPage>(&encoded))
        .transpose()?;
    if let Some(cached) = &cached {
        let page = &cached.page.payload;
        ensure!(
            cached.page.issuer == *local
                && page.version == AUTH_VERSION
                && page.kind == snapshot.kind
                && page.request_hash == snapshot.request_hash
                && page.snapshot_id == snapshot.snapshot_id
                && page.target_heads_hash == recovery_target_heads_hash(&heads)?
                && page.page_index.checked_add(1) == Some(record.position.page_count)
                && cached.page.hash()? == record.position.previous_page_hash
                && page.next_cursor == record.position.cursor
                && page.complete == record.witness.is_some(),
            "recovery cursor changed its signed page"
        );
        cached.page.verify_signature()?;
        if !page.complete {
            ensure!(
                page.next_cursor.as_ref()
                    == Some(&cursor_hash(
                        &record.snapshot,
                        &record.snapshot_salt,
                        &record.position,
                        &page.previous_page_hash,
                    )?),
                "recovery cursor changed its private position"
            );
        }
    } else {
        ensure!(
            record.position.cursor.is_none()
                && record.position.journal_cursor.is_none()
                && record.position.page_count == 0
                && record.position.entry_count == 0
                && record.position.previous_page_hash == record.snapshot.hash()?
                && record.witness.is_none(),
            "recovery initial cursor changed"
        );
    }
    if let Some(witness) = &record.witness {
        ensure!(
            witness.issuer == *local
                && witness.payload.version == AUTH_VERSION
                && witness.payload.kind == snapshot.kind
                && witness.payload.request_hash == snapshot.request_hash
                && witness.payload.snapshot_id == snapshot.snapshot_id
                && witness.payload.target_heads == heads
                && witness.payload.final_page_hash == record.position.previous_page_hash
                && witness.payload.page_count == record.position.page_count
                && witness.payload.entry_count == record.position.entry_count
                && witness.payload.claim_hash == snapshot.claim.hash()?
                && witness.payload.source_node_public_key == *local
                && snapshot.issued_at_ms <= witness.payload.issued_at_ms
                && witness.payload.issued_at_ms <= now
                && witness.payload.expires_at_ms == snapshot.expires_at_ms,
            "recovery completion witness changed"
        );
        witness.verify_signature()?;
    }
    Ok(cached)
}

fn check_read_usage(
    policy: &PolicyState,
    record: &RecoveryReadRecord,
    previous: Option<&CachedRecoveryPage>,
    outgoing: Option<&Signed<RecoveryContextPage>>,
    carrier_bytes: usize,
) -> Result<()> {
    let mut usage = RecoveryProofUsage::new(carrier_bytes)?;
    usage.record_request(&record.request)?;
    usage.record_snapshot(&record.snapshot)?;
    usage.record_current_source(&record.request, policy.verified())?;
    if let Some(previous) = previous {
        usage.record_page_binding(&previous.page)?;
    }
    if let Some(outgoing) = outgoing {
        usage.record_outgoing_page(outgoing)?;
    }
    if let Some(witness) = &record.witness {
        usage.record_witness(witness)?;
    }
    let _counts = usage.finish()?;
    #[cfg(test)]
    eprintln!(
        "bounded_recovery_source_proof {}",
        serde_json::to_string(&_counts)?
    );
    Ok(())
}

impl Store {
    pub(crate) fn begin_recovery_read(
        &mut self,
        identity: &LocalIdentity,
        principal: &str,
        request: RecoveryReadRequest,
        now: u64,
    ) -> Result<Value> {
        let _verification = ScopedSignatureVerification::new();
        bounded_value(serde_json::to_value(&request)?)?;
        let signer = signer(identity)?;
        let local = PublicKey::of(&signer);
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let heads = verify_request(&self.policy, &request, &local, principal, now)?;
        check_heads_in(&tx, &heads)?;
        let nonce_key = request_key(&request);
        if let Some(id) = metadata(&tx, &nonce_key)? {
            let id = Nonce::parse(&id)?;
            let encoded =
                metadata(&tx, &context_key(&id))?.context("recovery snapshot is missing")?;
            let record: RecoveryReadRecord = serde_json::from_str(&encoded)?;
            ensure!(
                record.snapshot.payload.snapshot_id == id && record.request == request,
                "recovery nonce was reused with another request"
            );
            let cached = verify_record(&tx, &self.policy, &record, &local, principal, now)?;
            let response = bounded_value(json!({"snapshot":record.snapshot}))?;
            check_read_usage(
                &self.policy,
                &record,
                cached.as_ref(),
                None,
                serde_json::to_vec(&record.request)?
                    .len()
                    .max(serde_json::to_vec(&response)?.len()),
            )?;
            return Ok(response);
        }
        let journal_snapshot = self.policy.accepted_journal_snapshot(&tx)?;
        let snapshot_salt = Nonce::random()?;
        let request_hash = request.request_hash()?;
        let id = snapshot_id(&request_hash, &journal_snapshot, &snapshot_salt)?;
        let claim: Signed<NodeClaim> = match &request {
            RecoveryReadRequest::Account(_) => serde_json::from_str(
                &bootstrap::create_node_claim(
                    &signer,
                    request.nonce().as_str(),
                    now as f64,
                    request.expires_at_ms() as f64,
                )
                .map_err(anyhow::Error::msg)?,
            )?,
            RecoveryReadRequest::CrossNode(value) => value.payload.target_claim.clone(),
        };
        let snapshot = Signed::sign(
            RecoveryContextSnapshot {
                version: AUTH_VERSION,
                kind: request.kind(),
                request_hash,
                snapshot_id: id,
                saved_pins_hash: request.saved_pins_hash()?,
                target_heads: heads,
                source_node_public_key: local,
                claim,
                issued_at_ms: now,
                expires_at_ms: request.expires_at_ms(),
            },
            &signer,
        )?;
        let response = bounded_value(json!({"snapshot":snapshot}))?;
        let record = RecoveryReadRecord {
            request,
            snapshot: snapshot.clone(),
            snapshot_salt,
            journal_snapshot,
            position: RecoveryReadPosition {
                cursor: None,
                journal_cursor: None,
                page_count: 0,
                entry_count: 0,
                previous_page_hash: snapshot.hash()?,
            },
            witness: None,
            source_approval: None,
        };
        check_read_usage(
            &self.policy,
            &record,
            None,
            None,
            serde_json::to_vec(&record.request)?
                .len()
                .max(serde_json::to_vec(&response)?.len()),
        )?;
        tx.execute(
            "INSERT INTO identity_meta(key,value) VALUES(?,?)",
            params![
                context_key(&snapshot.payload.snapshot_id),
                encode_record(&record)?
            ],
        )?;
        tx.execute(
            "INSERT INTO identity_meta(key,value) VALUES(?,?)",
            params![nonce_key, snapshot.payload.snapshot_id.as_str()],
        )?;
        tx.commit()?;
        Ok(response)
    }

    pub(crate) fn recovery_context_page(
        &mut self,
        identity: &LocalIdentity,
        principal: &str,
        params: &Value,
        now: u64,
    ) -> Result<Value> {
        let _verification = ScopedSignatureVerification::new();
        bounded_value(params.clone())?;
        let request: RecoveryContextPageRequest = serde_json::from_value(params.clone())?;
        let signer = signer(identity)?;
        let local = PublicKey::of(&signer);
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let key = context_key(&request.snapshot_id);
        let encoded = metadata(&tx, &key)?.context("recovery snapshot is missing")?;
        let mut record: RecoveryReadRecord = serde_json::from_str(&encoded)?;
        ensure!(
            record.snapshot.payload.snapshot_id == request.snapshot_id
                && record.snapshot.payload.request_hash == request.request_hash,
            "recovery request changed"
        );
        let cached = verify_record(&tx, &self.policy, &record, &local, principal, now)?;
        if let Some(cached) = &cached
            && cached.request_cursor == request.cursor
        {
            let response = bounded_value(json!({"page":cached.page}))?;
            check_read_usage(
                &self.policy,
                &record,
                Some(cached),
                None,
                serde_json::to_vec(params)?
                    .len()
                    .max(serde_json::to_vec(&response)?.len()),
            )?;
            return Ok(response);
        }
        ensure!(
            record.witness.is_none() && request.cursor == record.position.cursor,
            "recovery cursor is stale or complete"
        );
        let domains = [
            PolicyDomain::Account(record.request.account().clone()),
            PolicyDomain::Space(record.request.space().clone()),
        ]
        .into_iter()
        .collect();
        let scanned = self.policy.accepted_journal_page(
            &tx,
            &record.journal_snapshot,
            record.position.journal_cursor.as_ref(),
            &domains,
        )?;
        ensure!(scanned.scanned <= 32, "recovery scan budget exceeded");
        let entry = scanned.entry.map(|entry| entry.entry);
        let previous_page_hash = record.position.previous_page_hash.clone();
        let page_index = record.position.page_count;
        record.position.page_count = page_index
            .checked_add(1)
            .context("recovery page count overflow")?;
        record.position.entry_count = record
            .position
            .entry_count
            .checked_add(u64::from(entry.is_some()))
            .context("recovery entry count overflow")?;
        record.position.journal_cursor = Some(scanned.next_cursor);
        let next_cursor = if scanned.complete {
            None
        } else {
            Some(cursor_hash(
                &record.snapshot,
                &record.snapshot_salt,
                &record.position,
                &previous_page_hash,
            )?)
        };
        let page = Signed::sign(
            RecoveryContextPage {
                version: AUTH_VERSION,
                kind: record.request.kind(),
                request_hash: request.request_hash,
                snapshot_id: request.snapshot_id,
                target_heads_hash: recovery_target_heads_hash(
                    &record.snapshot.payload.target_heads,
                )?,
                page_index,
                previous_page_hash,
                entry,
                next_cursor: next_cursor.clone(),
                complete: scanned.complete,
            },
            &signer,
        )?;
        let response = bounded_value(json!({"page":page}))?;
        record.position.cursor = next_cursor;
        record.position.previous_page_hash = page.hash()?;
        if scanned.complete {
            record.witness = Some(Signed::sign(
                RecoveryContextWitness {
                    version: AUTH_VERSION,
                    kind: record.request.kind(),
                    request_hash: record.request.request_hash()?,
                    snapshot_id: record.snapshot.payload.snapshot_id.clone(),
                    target_heads: record.snapshot.payload.target_heads.clone(),
                    final_page_hash: page.hash()?,
                    page_count: record.position.page_count,
                    entry_count: record.position.entry_count,
                    claim_hash: record.snapshot.payload.claim.hash()?,
                    source_node_public_key: local,
                    issued_at_ms: now,
                    expires_at_ms: record.request.expires_at_ms(),
                },
                &signer,
            )?);
        }
        check_read_usage(
            &self.policy,
            &record,
            cached.as_ref(),
            Some(&page),
            serde_json::to_vec(params)?
                .len()
                .max(serde_json::to_vec(&response)?.len()),
        )?;
        let cached = CachedRecoveryPage {
            request_cursor: request.cursor,
            page,
        };
        tx.execute(
            "INSERT INTO identity_meta(key,value) VALUES(?,?) ON CONFLICT(key) DO UPDATE SET value=excluded.value",
            params![page_key(&record.snapshot.payload.snapshot_id), encode_record(&cached)?],
        )?;
        ensure!(
            tx.execute(
                "UPDATE identity_meta SET value=? WHERE key=? AND value=?",
                params![encode_record(&record)?, key, encoded],
            )? == 1,
            "recovery cursor changed concurrently"
        );
        tx.commit()?;
        Ok(response)
    }

    pub(crate) fn recovery_context_witness(
        &mut self,
        identity: &LocalIdentity,
        principal: &str,
        params: &Value,
        now: u64,
    ) -> Result<Value> {
        let _verification = ScopedSignatureVerification::new();
        bounded_value(params.clone())?;
        let request: RecoveryContextPageRequest = serde_json::from_value(params.clone())?;
        ensure!(
            request.cursor.is_none(),
            "recovery witness does not accept a cursor"
        );
        let local = PublicKey::parse(&identity.public_key_hex()?)?;
        let tx = self.connection.transaction()?;
        let record: RecoveryReadRecord = serde_json::from_str(
            &metadata(&tx, &context_key(&request.snapshot_id))?
                .context("recovery snapshot is missing")?,
        )?;
        ensure!(
            record.snapshot.payload.snapshot_id == request.snapshot_id
                && record.snapshot.payload.request_hash == request.request_hash,
            "recovery request changed"
        );
        let cached = verify_record(&tx, &self.policy, &record, &local, principal, now)?;
        let response = bounded_value(
            json!({"witness":record.witness.as_ref().context("recovery import is not complete")?}),
        )?;
        check_read_usage(
            &self.policy,
            &record,
            cached.as_ref(),
            None,
            serde_json::to_vec(params)?
                .len()
                .max(serde_json::to_vec(&response)?.len()),
        )?;
        Ok(response)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::identity::test_support::{TestNetwork, now};
    use vibeface_secure::{
        authorization::{
            JournalEntry,
            bootstrap::recovery::{self, context},
        },
        signature_verification::SignatureVerification,
    };

    fn read_request(network: &TestNetwork, client: &Identity) -> Result<RecoveryReadRequest> {
        let material = recovery::build_account_recovery_material(
            &network.created,
            &network.root,
            &network.recovery,
            "https://node.example",
            now() as f64,
        )
        .map_err(anyhow::Error::msg)?;
        let request = context::build_recovery_context_request(
            &material,
            client,
            &network.node.public_key_hex()?,
            now() as f64,
            (now() + 90_000) as f64,
        )
        .map_err(anyhow::Error::msg)?;
        Ok(RecoveryReadRequest::Account(serde_json::from_str(
            &request,
        )?))
    }

    fn begin(
        store: &mut Store,
        network: &TestNetwork,
        client: &Identity,
        request: RecoveryReadRequest,
    ) -> Result<Signed<RecoveryContextSnapshot>> {
        let verification = SignatureVerification::new();
        let response = verification.run(|| {
            store.begin_recovery_read(&network.node, &client.public_key_hex(), request, now())
        })?;
        assert!(verification.counts().actual_verifications <= 32);
        assert!(serde_json::to_vec(&response)?.len() <= MAX_CARRIER_BYTES);
        Ok(serde_json::from_value(response["snapshot"].clone())?)
    }

    fn page_params(
        snapshot: &Signed<RecoveryContextSnapshot>,
        cursor: Option<ObjectHash>,
    ) -> Result<Value> {
        Ok(serde_json::to_value(RecoveryContextPageRequest {
            snapshot_id: snapshot.payload.snapshot_id.clone(),
            request_hash: snapshot.payload.request_hash.clone(),
            cursor,
        })?)
    }

    #[test]
    fn pages_are_exact_bounded_retryable_and_survive_reopen() -> Result<()> {
        let network = TestNetwork::new()?;
        let path = network.directory.join("bounded-recovery.sqlite3");
        let mut store = network.at(&path)?;
        let client = Identity::generate().map_err(anyhow::Error::msg)?;
        let request = read_request(&network, &client)?;
        let snapshot = begin(&mut store, &network, &client, request.clone())?;
        let repeated = begin(&mut store, &network, &client, request)?;
        assert_eq!(snapshot, repeated);
        let first = page_params(&snapshot, None)?;
        let response =
            store.recovery_context_page(&network.node, &client.public_key_hex(), &first, now())?;
        drop(store);
        let mut store = Store::open(&path)?;
        assert_eq!(
            response,
            store.recovery_context_page(&network.node, &client.public_key_hex(), &first, now())?
        );
        let mut page: Signed<RecoveryContextPage> =
            serde_json::from_value(response["page"].clone())?;
        let mut entries: Vec<JournalEntry> = page.payload.entry.clone().into_iter().collect();
        let mut count = 1;
        while !page.payload.complete {
            let params = page_params(&snapshot, page.payload.next_cursor.clone())?;
            let verification = SignatureVerification::new();
            let response = verification.run(|| {
                store.recovery_context_page(&network.node, &client.public_key_hex(), &params, now())
            })?;
            assert!(verification.counts().actual_verifications <= 32);
            assert!(serde_json::to_vec(&response)?.len() <= MAX_CARRIER_BYTES);
            let next: Signed<RecoveryContextPage> =
                serde_json::from_value(response["page"].clone())?;
            assert_eq!(next.payload.previous_page_hash, page.hash()?);
            assert_eq!(next.payload.page_index, count);
            entries.extend(next.payload.entry.clone());
            page = next;
            count += 1;
        }
        assert_eq!(entries, network.bundle.journal);
        let params = page_params(&snapshot, None)?;
        let response = store.recovery_context_witness(
            &network.node,
            &client.public_key_hex(),
            &params,
            now(),
        )?;
        let witness: Signed<RecoveryContextWitness> =
            serde_json::from_value(response["witness"].clone())?;
        assert_eq!(witness.payload.page_count, count);
        assert_eq!(witness.payload.entry_count as usize, entries.len());
        assert_eq!(witness.payload.final_page_hash, page.hash()?);
        assert_eq!(
            response,
            store.recovery_context_witness(
                &network.node,
                &client.public_key_hex(),
                &params,
                now()
            )?
        );
        Ok(())
    }

    #[test]
    fn cursor_metadata_cannot_skip_history_and_wrong_noise_cannot_read() -> Result<()> {
        let network = TestNetwork::new()?;
        let mut store = network.at(&network.directory.join("cursor.sqlite3"))?;
        let client = Identity::generate().map_err(anyhow::Error::msg)?;
        let snapshot = begin(
            &mut store,
            &network,
            &client,
            read_request(&network, &client)?,
        )?;
        let params = page_params(&snapshot, None)?;
        let outsider = Identity::generate().map_err(anyhow::Error::msg)?;
        assert!(
            store
                .recovery_context_page(&network.node, &outsider.public_key_hex(), &params, now())
                .is_err()
        );
        assert!(
            store
                .recovery_context_witness(&network.node, &client.public_key_hex(), &params, now())
                .is_err()
        );
        let response =
            store.recovery_context_page(&network.node, &client.public_key_hex(), &params, now())?;
        let page: Signed<RecoveryContextPage> = serde_json::from_value(response["page"].clone())?;
        assert!(!page.payload.complete);
        for index in 1..=32_u64 {
            let public_guess = ObjectHash::body(&(
                "recovery-read-cursor-v2",
                snapshot.hash()?,
                json!({"next_index":index,"after_ordinal":index}),
                1_u64,
                1_u64,
                snapshot.hash()?,
            ))?;
            assert_ne!(page.payload.next_cursor.as_ref(), Some(&public_guess));
        }
        let key = context_key(&snapshot.payload.snapshot_id);
        let original = metadata(&store.connection, &key)?.unwrap();
        let mut changed: Value = serde_json::from_str(&original)?;
        changed["position"]["entry_count"] = json!(999);
        store.connection.execute(
            "UPDATE identity_meta SET value=? WHERE key=?",
            params![serde_json::to_string(&changed)?, key],
        )?;
        let next = page_params(&snapshot, page.payload.next_cursor)?;
        assert!(
            store
                .recovery_context_page(&network.node, &client.public_key_hex(), &next, now())
                .is_err()
        );
        assert_eq!(
            metadata(&store.connection, &key)?.unwrap(),
            serde_json::to_string(&changed)?
        );
        Ok(())
    }

    #[test]
    fn snapshot_scope_nonce_reuse_and_changed_database_heads_are_rejected() -> Result<()> {
        let network = TestNetwork::new()?;
        let mut store = network.at(&network.directory.join("snapshot.sqlite3"))?;
        let client = Identity::generate().map_err(anyhow::Error::msg)?;
        let request = read_request(&network, &client)?;
        let snapshot = begin(&mut store, &network, &client, request.clone())?;
        let RecoveryReadRequest::Account(mut invalid) = request.clone() else {
            unreachable!()
        };
        invalid.payload.nonce = Nonce::random()?;
        invalid.signature = vibeface_secure::authorization::Signature::parse(&"00".repeat(64))?;
        assert!(
            store
                .begin_recovery_read(
                    &network.node,
                    &client.public_key_hex(),
                    RecoveryReadRequest::Account(invalid),
                    now(),
                )
                .is_err()
        );
        let RecoveryReadRequest::Account(mut changed) = request else {
            unreachable!()
        };
        let another = Identity::generate().map_err(anyhow::Error::msg)?;
        changed.payload.new_device_public_key = PublicKey::of(&another);
        let changed = Signed::sign(changed.payload, &network.recovery)?;
        assert!(
            store
                .begin_recovery_read(
                    &network.node,
                    &another.public_key_hex(),
                    RecoveryReadRequest::Account(changed),
                    now()
                )
                .is_err()
        );
        let key = context_key(&snapshot.payload.snapshot_id);
        let original = metadata(&store.connection, &key)?.unwrap();
        let mut changed: Value = serde_json::from_str(&original)?;
        changed["snapshot_salt"] = json!(Nonce::random()?);
        store.connection.execute(
            "UPDATE identity_meta SET value=? WHERE key=?",
            params![serde_json::to_string(&changed)?, key],
        )?;
        let params = page_params(&snapshot, None)?;
        assert!(
            store
                .recovery_context_page(&network.node, &client.public_key_hex(), &params, now())
                .is_err()
        );
        store.connection.execute(
            "UPDATE identity_meta SET value=? WHERE key=?",
            params![original, key],
        )?;
        store.connection.execute("UPDATE current_policy_heads SET sequence=sequence+1 WHERE domain_kind='account' AND domain_id=?", [network.account.as_str()])?;
        assert!(
            store
                .recovery_context_page(&network.node, &client.public_key_hex(), &params, now())
                .is_err()
        );
        Ok(())
    }
}
