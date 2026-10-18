use super::journal::{AcceptedJournalCursor, AcceptedJournalSnapshot, PolicyState};
use crate::{crypto::LocalIdentity, store::Store};
use anyhow::{Context, Result, ensure};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use vibeface_secure::{
    Identity,
    authorization::{
        AUTH_VERSION, AuthorizationError, MAX_PROOF_BYTES, Nonce, ObjectHash, PolicyDomain,
        PolicyStamp, PublicKey, ResourceRef, ResourceScope, Signed, SignedObject,
        client_policy_import::*, execution_handoff::ExecutionProofUsage,
        policy_proof::policy_entry_payload_usage,
    },
    signature_verification::ScopedSignatureVerification,
};

const MAX_STORED_BYTES: usize = MAX_PROOF_BYTES * 2;

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ClientReadRecord {
    request: Signed<ClientPolicyReadRequest>,
    snapshot: Signed<ClientPolicySnapshot>,
    journal_snapshot: AcceptedJournalSnapshot,
    snapshot_salt: Nonce,
    position: ReadPosition,
    witness: Option<Signed<ClientPolicyWitness>>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ReadPosition {
    cursor: Option<ObjectHash>,
    journal_cursor: Option<AcceptedJournalCursor>,
    page_count: u64,
    entry_count: u64,
    previous_page_hash: ObjectHash,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CachedPage {
    request_cursor: Option<ObjectHash>,
    page: Signed<ClientPolicyPage>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SelectionsRecord {
    request: Signed<ClientPolicyReadRequest>,
    snapshot_id: Nonce,
    heads: Vec<PolicyStamp>,
    grant_index: usize,
    resource_index: usize,
    cursor: Option<ObjectHash>,
    complete: bool,
    cached: Option<CachedSelections>,
    first_page: Option<Signed<ClientPolicySelectionsPage>>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CachedSelections {
    request_cursor: Option<ObjectHash>,
    page: Signed<ClientPolicySelectionsPage>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RequestBinding {
    request_hash: ObjectHash,
    snapshot_id: Nonce,
    selected: bool,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ConfirmationRecord {
    request: Signed<ClientPolicyAuthConfirmRequest>,
    confirmation: Signed<ClientPolicyAuthConfirmation>,
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
        length.is_none_or(|length| length <= MAX_STORED_BYTES),
        "stored client policy context is too large"
    );
    Ok(connection
        .query_row(
            "SELECT value FROM identity_meta WHERE key=?",
            [key],
            |row| row.get(0),
        )
        .optional()?)
}

fn encode<T: Serialize>(value: &T) -> Result<String> {
    let encoded = serde_json::to_string(value)?;
    ensure!(
        encoded.len() <= MAX_STORED_BYTES,
        "stored client policy context is too large"
    );
    Ok(encoded)
}

fn bounded(value: Value) -> Result<Value> {
    ensure!(
        serde_json::to_vec(&value)?.len() <= MAX_PROOF_BYTES,
        "client policy carrier exceeds 64 KiB"
    );
    Ok(value)
}

fn finish_response(value: Value, mut usage: ExecutionProofUsage) -> Result<Value> {
    if let Some(counts) =
        vibeface_secure::signature_verification::SignatureVerification::current_counts()
    {
        usage.record_actual_signature_counts(counts);
    }
    usage.enforce_limits()?;
    #[cfg(test)]
    eprintln!("client_policy proof_usage={:?}", usage.counts());
    bounded(value)
}

fn signed_usage<T: SignedObject>(value: &Signed<T>) -> Result<ExecutionProofUsage> {
    let mut usage = ExecutionProofUsage::default();
    usage.record_signed(value)?;
    Ok(usage)
}

fn signer(identity: &LocalIdentity) -> Result<Identity> {
    Identity::from_secret_hex(&identity.secret_key_hex).map_err(anyhow::Error::msg)
}

fn context_key(id: &Nonce) -> String {
    format!("client-policy-read:{id}")
}
fn page_key(id: &Nonce) -> String {
    format!("client-policy-page:{id}")
}
fn selections_key(id: &Nonce) -> String {
    format!("client-policy-selections:{id}")
}
fn confirmation_key(id: &Nonce) -> String {
    format!("client-policy-confirmation:{id}")
}
fn request_key(request: &Signed<ClientPolicyReadRequest>) -> String {
    format!(
        "client-policy-request:{}:{}",
        request.payload.device_public_key, request.payload.nonce
    )
}

fn request_hash(request: &Signed<ClientPolicyReadRequest>) -> Result<ObjectHash> {
    Ok(request.hash()?)
}

fn snapshot_id(
    request: &ObjectHash,
    journal: &AcceptedJournalSnapshot,
    salt: &Nonce,
) -> Result<Nonce> {
    let hash = ObjectHash::body(&("client-policy-snapshot-id-v2", request, journal, salt))?;
    Ok(Nonce::parse(
        hash.as_str()
            .strip_prefix("h2_")
            .context("invalid client policy snapshot hash")?,
    )?)
}

fn check_heads_in(
    connection: &Connection,
    policy: &PolicyState,
    heads: &[PolicyStamp],
) -> Result<()> {
    ensure!(
        heads.len() <= vibeface_secure::authorization::MAX_POLICY_DOMAINS
            && heads.windows(2).all(|pair| pair[0].domain < pair[1].domain),
        "invalid client policy dependency closure"
    );
    for head in heads {
        ensure!(
            policy.verified().head(&head.domain) == Some(head),
            "client policy_changed"
        );
        let pin = policy
            .pins()
            .iter()
            .find(|pin| pin.domain == head.domain)
            .context("client policy dependency has no trusted genesis")?;
        let (kind, id) = match &head.domain {
            PolicyDomain::Account(id) => ("account", id.as_str()),
            PolicyDomain::Space(id) => ("space", id.as_str()),
        };
        let actual: Option<(u64, u64, String)> = connection.query_row(
            "SELECT authority_epoch,sequence,head_hash FROM current_policy_heads WHERE domain_kind=? AND domain_id=?",
            params![kind,id], |row| Ok((row.get(0)?,row.get(1)?,row.get(2)?)),
        ).optional()?;
        ensure!(
            actual == Some((head.authority_epoch, head.sequence, head.head.to_string())),
            "client policy_changed"
        );
        let stored: Option<(String, Option<String>)> = connection.query_row(
            "SELECT genesis_root,minimum_head FROM policy_pins WHERE domain_kind=? AND domain_id=?",
            params![kind,id], |row| Ok((row.get(0)?,row.get(1)?)),
        ).optional()?;
        let (root, minimum) = stored.context("client policy pin was removed")?;
        let minimum: Option<PolicyStamp> = minimum
            .map(|value| serde_json::from_str(&value))
            .transpose()?;
        ensure!(
            root == pin.genesis_root.as_str() && minimum == pin.minimum_head,
            "client policy pin changed"
        );
    }
    Ok(())
}

fn verify_read(
    connection: &Connection,
    policy: &PolicyState,
    request: &Signed<ClientPolicyReadRequest>,
    local: &PublicKey,
    principal: &str,
    now: u64,
) -> Result<VerifiedClientPolicyRead> {
    let verified = verify_client_policy_read(
        policy.verified(),
        request,
        local,
        &PublicKey::parse(principal)?,
        now,
    )?;
    check_heads_in(connection, policy, verified.heads())?;
    Ok(verified)
}

fn cursor_hash(
    snapshot: &Signed<ClientPolicySnapshot>,
    position: &ReadPosition,
    previous: &ObjectHash,
) -> Result<ObjectHash> {
    Ok(ObjectHash::body(&(
        "client-policy-cursor-v2",
        snapshot.hash()?,
        &position.journal_cursor,
        position.page_count,
        position.entry_count,
        previous,
    ))?)
}

fn verify_record(
    connection: &Connection,
    policy: &PolicyState,
    record: &ClientReadRecord,
    local: &PublicKey,
    principal: &str,
    now: u64,
) -> Result<(
    VerifiedClientPolicyRead,
    Option<CachedPage>,
    ExecutionProofUsage,
)> {
    let verified = verify_read(connection, policy, &record.request, local, principal, now)?;
    let snapshot = &record.snapshot.payload;
    ensure!(
        record.snapshot.issuer == *local
            && snapshot.version == AUTH_VERSION
            && snapshot.request_hash == request_hash(&record.request)?
            && snapshot.saved_pins_hash == ObjectHash::body(&record.request.payload.saved_pins)?
            && snapshot.snapshot_id
                == snapshot_id(
                    &snapshot.request_hash,
                    &record.journal_snapshot,
                    &record.snapshot_salt
                )?
            && snapshot.target_heads == verified.heads()
            && Some(&snapshot.selection) == verified.selection()
            && snapshot.source_node_public_key == *local
            && record.request.payload.issued_at_ms <= snapshot.issued_at_ms
            && snapshot.issued_at_ms <= now
            && now < snapshot.expires_at_ms
            && snapshot.expires_at_ms == record.request.payload.expires_at_ms,
        "client policy snapshot or selection changed"
    );
    record.snapshot.verify_signature()?;
    let mut usage = signed_usage(&record.request)?;
    usage.merge(verified.proof_usage());
    usage.record_signed(&record.snapshot)?;
    let cached: Option<CachedPage> = metadata(connection, &page_key(&snapshot.snapshot_id))?
        .map(|value| serde_json::from_str(&value))
        .transpose()?;
    if let Some(cached) = &cached {
        let page = &cached.page.payload;
        ensure!(
            cached.page.issuer == *local
                && page.version == AUTH_VERSION
                && page.request_hash == snapshot.request_hash
                && page.snapshot_id == snapshot.snapshot_id
                && page.target_heads_hash == ObjectHash::body(&snapshot.target_heads)?
                && page.page_index.checked_add(1) == Some(record.position.page_count)
                && cached.page.hash()? == record.position.previous_page_hash
                && page.next_cursor == record.position.cursor
                && page.complete == record.witness.is_some(),
            "client policy cursor changed its signed page"
        );
        cached.page.verify_signature()?;
        usage.record_signed(&cached.page)?;
        if let Some(entry) = &page.entry {
            usage.merge(&policy_entry_payload_usage(entry)?);
        }
        if !page.complete {
            ensure!(
                page.next_cursor.as_ref()
                    == Some(&cursor_hash(
                        &record.snapshot,
                        &record.position,
                        &page.previous_page_hash
                    )?),
                "client policy cursor changed its private position"
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
            "client policy initial cursor changed"
        );
    }
    if let Some(witness) = &record.witness {
        let value = &witness.payload;
        ensure!(
            witness.issuer == *local
                && value.version == AUTH_VERSION
                && value.request_hash == snapshot.request_hash
                && value.snapshot_id == snapshot.snapshot_id
                && value.target_heads == snapshot.target_heads
                && value.snapshot_hash == record.snapshot.hash()?
                && value.final_page_hash == record.position.previous_page_hash
                && value.page_count == record.position.page_count
                && value.entry_count == record.position.entry_count
                && value.source_node_public_key == *local
                && snapshot.issued_at_ms <= value.issued_at_ms
                && value.issued_at_ms <= now
                && value.expires_at_ms == snapshot.expires_at_ms,
            "client policy completion witness changed"
        );
        witness.verify_signature()?;
        usage.record_signed(witness)?;
    }
    Ok((verified, cached, usage))
}

impl Store {
    pub(crate) fn begin_client_policy_read(
        &mut self,
        identity: &LocalIdentity,
        principal: &str,
        request: Signed<ClientPolicyReadRequest>,
        now: u64,
    ) -> Result<Value> {
        let _verification = ScopedSignatureVerification::new();
        bounded(serde_json::to_value(&request)?)?;
        let signer = signer(identity)?;
        let local = PublicKey::of(&signer);
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let verified = verify_read(&tx, &self.policy, &request, &local, principal, now)?;
        let nonce_key = request_key(&request);
        let hash = request_hash(&request)?;
        if let Some(binding) = metadata(&tx, &nonce_key)? {
            let binding: RequestBinding = serde_json::from_str(&binding)?;
            ensure!(
                binding.request_hash == hash
                    && binding.selected == request.payload.selection.is_some(),
                "client policy nonce was reused with another request"
            );
            if binding.selected {
                let record: ClientReadRecord = serde_json::from_str(
                    &metadata(&tx, &context_key(&binding.snapshot_id))?
                        .context("client policy snapshot is missing")?,
                )?;
                ensure!(
                    record.request == request
                        && record.snapshot.payload.snapshot_id == binding.snapshot_id,
                    "client policy request binding changed"
                );
                let (_, _, usage) =
                    verify_record(&tx, &self.policy, &record, &local, principal, now)?;
                return finish_response(json!({"snapshot":record.snapshot}), usage);
            }
            let record: SelectionsRecord = serde_json::from_str(
                &metadata(&tx, &selections_key(&binding.snapshot_id))?
                    .context("client policy selections are missing")?,
            )?;
            ensure!(
                record.request == request && record.snapshot_id == binding.snapshot_id,
                "client policy request binding changed"
            );
            check_heads_in(&tx, &self.policy, &record.heads)?;
            let page = record
                .first_page
                .context("client policy selections have no first page")?;
            let page_usage = verify_selections_page(
                &tx,
                &self.policy,
                &record.request,
                &binding.snapshot_id,
                &page,
                &local,
                principal,
                now,
            )?;
            let mut usage = signed_usage(&request)?;
            usage.merge(verified.proof_usage());
            usage.merge(&page_usage);
            usage.record_signed(&page)?;
            return finish_response(json!({"selections":page}), usage);
        }
        let selected = verified.selection().cloned();
        let journal_snapshot = selected
            .as_ref()
            .map(|_| self.policy.accepted_journal_snapshot(&tx))
            .transpose()?;
        let snapshot_salt = Nonce::random()?;
        let id = match &journal_snapshot {
            Some(journal) => snapshot_id(&hash, journal, &snapshot_salt)?,
            None => Nonce::random()?,
        };
        let response = if let Some(selection) = selected {
            let snapshot = Signed::sign(
                ClientPolicySnapshot {
                    version: AUTH_VERSION,
                    request_hash: hash.clone(),
                    snapshot_id: id.clone(),
                    saved_pins_hash: ObjectHash::body(&request.payload.saved_pins)?,
                    target_heads: verified.heads().to_vec(),
                    selection,
                    source_node_public_key: local,
                    issued_at_ms: now,
                    expires_at_ms: request.payload.expires_at_ms,
                },
                &signer,
            )?;
            let record = ClientReadRecord {
                request: request.clone(),
                journal_snapshot: journal_snapshot.context("client policy snapshot is missing")?,
                snapshot_salt,
                position: ReadPosition {
                    cursor: None,
                    journal_cursor: None,
                    page_count: 0,
                    entry_count: 0,
                    previous_page_hash: snapshot.hash()?,
                },
                snapshot: snapshot.clone(),
                witness: None,
            };
            let mut usage = signed_usage(&request)?;
            usage.merge(verified.proof_usage());
            usage.record_signed(&snapshot)?;
            let response = finish_response(json!({"snapshot":snapshot}), usage)?;
            tx.execute(
                "INSERT INTO identity_meta(key,value) VALUES(?,?)",
                params![context_key(&id), encode(&record)?],
            )?;
            response
        } else {
            let mut heads = verified.heads().to_vec();
            let domain = PolicyDomain::Space(request.payload.space.clone());
            if !heads.iter().any(|head| head.domain == domain) {
                heads.push(
                    self.policy
                        .verified()
                        .head(&domain)
                        .context("client catalog space is missing")?
                        .clone(),
                );
                heads.sort_by(|a, b| a.domain.cmp(&b.domain));
            }
            check_heads_in(&tx, &self.policy, &heads)?;
            let mut record = SelectionsRecord {
                request: request.clone(),
                snapshot_id: id.clone(),
                heads,
                grant_index: 0,
                resource_index: 0,
                cursor: None,
                complete: false,
                cached: None,
                first_page: None,
            };
            let response = next_selections_in(
                &tx,
                &self.policy,
                &mut record,
                None,
                &signer,
                principal,
                now,
            )?;
            tx.execute(
                "INSERT INTO identity_meta(key,value) VALUES(?,?)",
                params![selections_key(&id), encode(&record)?],
            )?;
            response
        };
        tx.execute(
            "INSERT INTO identity_meta(key,value) VALUES(?,?)",
            params![
                nonce_key,
                encode(&RequestBinding {
                    request_hash: hash,
                    snapshot_id: id,
                    selected: request.payload.selection.is_some()
                })?
            ],
        )?;
        tx.commit()?;
        Ok(response)
    }

    pub(crate) fn client_policy_page(
        &mut self,
        identity: &LocalIdentity,
        principal: &str,
        params: &Value,
        now: u64,
    ) -> Result<Value> {
        let _verification = ScopedSignatureVerification::new();
        bounded(params.clone())?;
        let request: ClientPolicyPageRequest = serde_json::from_value(params.clone())?;
        let signer = signer(identity)?;
        let local = PublicKey::of(&signer);
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let key = context_key(&request.snapshot_id);
        let encoded = metadata(&tx, &key)?.context("client policy snapshot is missing")?;
        let mut record: ClientReadRecord = serde_json::from_str(&encoded)?;
        ensure!(
            record.snapshot.payload.snapshot_id == request.snapshot_id
                && record.snapshot.payload.request_hash == request.request_hash,
            "client policy page request changed"
        );
        let (verified, cached, mut usage) =
            verify_record(&tx, &self.policy, &record, &local, principal, now)?;
        if let Some(cached) = cached {
            if cached.request_cursor == request.cursor {
                return finish_response(json!({"page":cached.page}), usage);
            }
        }
        ensure!(
            record.witness.is_none() && request.cursor == record.position.cursor,
            "client policy cursor is stale or complete"
        );
        let scanned = self.policy.accepted_journal_page(
            &tx,
            &record.journal_snapshot,
            record.position.journal_cursor.as_ref(),
            &verified.domains(),
        )?;
        ensure!(scanned.scanned <= 32, "client policy scan budget exceeded");
        let entry = scanned.entry.map(|value| value.entry);
        if let Some(entry) = &entry {
            usage.merge(&policy_entry_payload_usage(entry)?);
        }
        let previous_page_hash = record.position.previous_page_hash.clone();
        let page_index = record.position.page_count;
        record.position.page_count = page_index
            .checked_add(1)
            .context("client policy page count overflow")?;
        record.position.entry_count = record
            .position
            .entry_count
            .checked_add(u64::from(entry.is_some()))
            .context("client policy entry count overflow")?;
        record.position.journal_cursor = Some(scanned.next_cursor);
        let next_cursor = if scanned.complete {
            None
        } else {
            Some(cursor_hash(
                &record.snapshot,
                &record.position,
                &previous_page_hash,
            )?)
        };
        let page = Signed::sign(
            ClientPolicyPage {
                version: AUTH_VERSION,
                request_hash: request.request_hash,
                snapshot_id: request.snapshot_id,
                target_heads_hash: ObjectHash::body(&record.snapshot.payload.target_heads)?,
                page_index,
                previous_page_hash,
                entry,
                next_cursor: next_cursor.clone(),
                complete: scanned.complete,
            },
            &signer,
        )?;
        usage.record_signed(&page)?;
        record.position.cursor = next_cursor;
        record.position.previous_page_hash = page.hash()?;
        if scanned.complete {
            let witness = Signed::sign(
                ClientPolicyWitness {
                    version: AUTH_VERSION,
                    request_hash: request_hash(&record.request)?,
                    snapshot_id: record.snapshot.payload.snapshot_id.clone(),
                    target_heads: record.snapshot.payload.target_heads.clone(),
                    final_page_hash: page.hash()?,
                    page_count: record.position.page_count,
                    entry_count: record.position.entry_count,
                    snapshot_hash: record.snapshot.hash()?,
                    source_node_public_key: local,
                    issued_at_ms: now,
                    expires_at_ms: record.request.payload.expires_at_ms,
                },
                &signer,
            )?;
            usage.record_signed(&witness)?;
            record.witness = Some(witness);
        }
        let response = finish_response(json!({"page":page}), usage)?;
        tx.execute("INSERT INTO identity_meta(key,value) VALUES(?,?) ON CONFLICT(key) DO UPDATE SET value=excluded.value",
            params![page_key(&record.snapshot.payload.snapshot_id),encode(&CachedPage {request_cursor:request.cursor,page})?])?;
        ensure!(
            tx.execute(
                "UPDATE identity_meta SET value=? WHERE key=? AND value=?",
                params![encode(&record)?, key, encoded]
            )? == 1,
            "client policy cursor changed concurrently"
        );
        tx.commit()?;
        Ok(response)
    }

    pub(crate) fn client_policy_witness(
        &mut self,
        identity: &LocalIdentity,
        principal: &str,
        params: &Value,
        now: u64,
    ) -> Result<Value> {
        let _verification = ScopedSignatureVerification::new();
        bounded(params.clone())?;
        let request: ClientPolicyPageRequest = serde_json::from_value(params.clone())?;
        ensure!(
            request.cursor.is_none(),
            "client policy witness does not accept a cursor"
        );
        let local = PublicKey::parse(&identity.public_key_hex()?)?;
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let record: ClientReadRecord = serde_json::from_str(
            &metadata(&tx, &context_key(&request.snapshot_id))?
                .context("client policy snapshot is missing")?,
        )?;
        ensure!(
            record.snapshot.payload.snapshot_id == request.snapshot_id
                && record.snapshot.payload.request_hash == request.request_hash,
            "client policy witness request changed"
        );
        let (_, _, usage) = verify_record(&tx, &self.policy, &record, &local, principal, now)?;
        finish_response(
            json!({"witness":record.witness.context("client policy import is not complete")?}),
            usage,
        )
    }

    pub(crate) fn client_policy_selections(
        &mut self,
        identity: &LocalIdentity,
        principal: &str,
        params: &Value,
        now: u64,
    ) -> Result<Value> {
        let _verification = ScopedSignatureVerification::new();
        bounded(params.clone())?;
        let request: ClientPolicyPageRequest = serde_json::from_value(params.clone())?;
        let signer = signer(identity)?;
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let key = selections_key(&request.snapshot_id);
        let encoded = metadata(&tx, &key)?.context("client policy selections are missing")?;
        let mut record: SelectionsRecord = serde_json::from_str(&encoded)?;
        ensure!(
            record.snapshot_id == request.snapshot_id
                && request_hash(&record.request)? == request.request_hash,
            "client policy selections request changed"
        );
        let response = next_selections_in(
            &tx,
            &self.policy,
            &mut record,
            request.cursor,
            &signer,
            principal,
            now,
        )?;
        ensure!(
            tx.execute(
                "UPDATE identity_meta SET value=? WHERE key=? AND value=?",
                params![encode(&record)?, key, encoded]
            )? == 1,
            "client policy selections changed concurrently"
        );
        tx.commit()?;
        Ok(response)
    }

    pub(crate) fn confirm_client_policy_auth(
        &mut self,
        identity: &LocalIdentity,
        principal: &str,
        params: &Value,
        now: u64,
    ) -> Result<Value> {
        let _verification = ScopedSignatureVerification::new();
        bounded(params.clone())?;
        let request: Signed<ClientPolicyAuthConfirmRequest> =
            serde_json::from_value(params.clone())?;
        let signer = signer(identity)?;
        let local = PublicKey::of(&signer);
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let record: ClientReadRecord = serde_json::from_str(
            &metadata(&tx, &context_key(&request.payload.context.snapshot_id))?
                .context("client policy snapshot is missing")?,
        )?;
        let (_, _, mut usage) = verify_record(&tx, &self.policy, &record, &local, principal, now)?;
        let witness = record
            .witness
            .as_ref()
            .context("client policy import is not complete")?;
        verify_client_policy_auth_confirm(
            self.policy.verified(),
            &record.request,
            &record.snapshot,
            witness,
            &request,
            &local,
            &PublicKey::parse(principal)?,
            now,
        )?;
        usage.record_signed(&request)?;
        let key = confirmation_key(&record.snapshot.payload.snapshot_id);
        if let Some(previous) = metadata(&tx, &key)? {
            let previous: ConfirmationRecord = serde_json::from_str(&previous)?;
            ensure!(
                previous.request == request,
                "client policy confirmation changed its original request"
            );
            let confirmed = &previous.confirmation.payload;
            ensure!(
                previous.confirmation.issuer == local
                    && confirmed.version == AUTH_VERSION
                    && confirmed.context == request.payload.context
                    && confirmed.confirmation_request_hash == request.hash()?
                    && confirmed.selection == request.payload.selection
                    && confirmed.target_heads == request.payload.target_heads
                    && confirmed.device_public_key == request.payload.device_public_key
                    && confirmed.source_node_public_key == local
                    && record.snapshot.payload.issued_at_ms <= confirmed.accepted_at_ms
                    && confirmed.accepted_at_ms <= now,
                "client policy confirmation receipt changed"
            );
            previous.confirmation.verify_signature()?;
            usage.record_signed(&previous.confirmation)?;
            return finish_response(json!({"confirmation":previous.confirmation}), usage);
        }
        let confirmation = Signed::sign(
            ClientPolicyAuthConfirmation {
                version: AUTH_VERSION,
                context: request.payload.context.clone(),
                confirmation_request_hash: request.hash()?,
                selection: request.payload.selection.clone(),
                target_heads: request.payload.target_heads.clone(),
                device_public_key: request.payload.device_public_key.clone(),
                source_node_public_key: local,
                accepted_at_ms: now,
            },
            &signer,
        )?;
        usage.record_signed(&confirmation)?;
        let response = finish_response(json!({"confirmation":confirmation}), usage)?;
        tx.execute(
            "INSERT INTO identity_meta(key,value) VALUES(?,?)",
            params![
                key,
                encode(&ConfirmationRecord {
                    request,
                    confirmation
                })?
            ],
        )?;
        tx.commit()?;
        Ok(response)
    }
}

fn next_selections_in(
    connection: &Connection,
    policy: &PolicyState,
    record: &mut SelectionsRecord,
    cursor: Option<ObjectHash>,
    signer: &Identity,
    principal: &str,
    now: u64,
) -> Result<Value> {
    let local = PublicKey::of(signer);
    let base = verify_read(connection, policy, &record.request, &local, principal, now)?;
    ensure!(
        base.selection().is_none(),
        "client policy selections require an unselected request"
    );
    check_heads_in(connection, policy, &record.heads)?;
    let mut usage = signed_usage(&record.request)?;
    usage.merge(base.proof_usage());
    if let Some(cached) = &record.cached {
        usage.merge(&verify_selections_page(
            connection,
            policy,
            &record.request,
            &record.snapshot_id,
            &cached.page,
            &local,
            principal,
            now,
        )?);
        ensure!(
            cached.page.payload.next_cursor == record.cursor
                && cached.page.payload.next_cursor.is_none() == record.complete,
            "client policy selections cursor changed its signed page"
        );
        if !record.complete {
            ensure!(
                record.cursor.as_ref() == Some(&selections_cursor(record)?),
                "client policy selections cursor changed its private position"
            );
        }
        if cached.request_cursor == cursor {
            usage.record_signed(&cached.page)?;
            return finish_response(json!({"selections":cached.page}), usage);
        }
    } else {
        ensure!(
            record.cursor.is_none()
                && record.grant_index == 0
                && record.resource_index == 0
                && !record.complete
                && record.first_page.is_none(),
            "client policy initial selections cursor changed"
        );
    }
    ensure!(
        !record.complete && record.cursor == cursor,
        "client policy selections cursor is stale or complete"
    );
    let document = policy
        .verified()
        .space(&record.request.payload.space)
        .context("client catalog space is missing")?;
    let mut selections = Vec::new();
    if let Some(hash) = document.grants.get(record.grant_index) {
        let grant = policy
            .verified()
            .grant(hash)
            .context("client catalog grant proof is missing")?;
        let candidate = match &grant.payload.scope {
            ResourceScope::Exact(resource) => {
                record.grant_index += 1;
                record.resource_index = 0;
                Some(resource.clone())
            }
            ResourceScope::WholeSpace(space) => {
                let resource = document
                    .resources
                    .get(record.resource_index)
                    .map(|hash| -> Result<ResourceRef> {
                        let accepted = client_policy_resource(policy.verified(), hash)
                            .context("client catalog resource proof is missing")?;
                        ensure!(
                            accepted.payload.space == *space && accepted.hash()? == *hash,
                            "client catalog resource commitment changed"
                        );
                        Ok(ResourceRef {
                            space: space.clone(),
                            resource: accepted.payload.resource.clone(),
                        })
                    })
                    .transpose()?;
                record.resource_index += 1;
                if record.resource_index >= document.resources.len() {
                    record.grant_index += 1;
                    record.resource_index = 0;
                }
                resource
            }
        };
        if let Some(resource) = candidate {
            let selection = AuthorizationSelection {
                resource,
                audience: grant.payload.audience.clone(),
                grant_leaf: hash.clone(),
            };
            match verify_client_policy_read_selection(
                policy.verified(),
                &record.request,
                &selection,
                &local,
                &PublicKey::parse(principal)?,
                now,
            ) {
                Ok(verified) => {
                    check_heads_in(connection, policy, verified.heads())?;
                    usage.merge(verified.proof_usage());
                    selections.push(selection);
                }
                Err(
                    AuthorizationError::InvalidProof(_)
                    | AuthorizationError::Revoked
                    | AuthorizationError::Expired,
                ) => {}
                Err(error) => return Err(error.into()),
            }
        }
    }
    record.complete = record.grant_index >= document.grants.len();
    let next_cursor = if record.complete {
        None
    } else {
        Some(selections_cursor(record)?)
    };
    let page = Signed::sign(
        ClientPolicySelectionsPage {
            version: AUTH_VERSION,
            request_hash: request_hash(&record.request)?,
            snapshot_id: record.snapshot_id.clone(),
            saved_pins_hash: ObjectHash::body(&record.request.payload.saved_pins)?,
            selections,
            next_cursor: next_cursor.clone(),
            source_node_public_key: local,
            issued_at_ms: now,
            expires_at_ms: record.request.payload.expires_at_ms,
        },
        signer,
    )?;
    usage.record_signed(&page)?;
    let response = finish_response(json!({"selections":page}), usage)?;
    if record.first_page.is_none() {
        record.first_page = Some(page.clone());
    }
    record.cursor = next_cursor;
    record.cached = Some(CachedSelections {
        request_cursor: cursor,
        page,
    });
    Ok(response)
}

fn selections_cursor(record: &SelectionsRecord) -> Result<ObjectHash> {
    Ok(ObjectHash::body(&(
        "client-policy-selections-cursor-v2",
        &record.snapshot_id,
        request_hash(&record.request)?,
        &record.heads,
        record.grant_index,
        record.resource_index,
    ))?)
}

fn verify_selections_page(
    connection: &Connection,
    policy: &PolicyState,
    request: &Signed<ClientPolicyReadRequest>,
    snapshot_id: &Nonce,
    page: &Signed<ClientPolicySelectionsPage>,
    local: &PublicKey,
    principal: &str,
    now: u64,
) -> Result<ExecutionProofUsage> {
    let value = &page.payload;
    ensure!(
        page.issuer == *local
            && value.version == AUTH_VERSION
            && value.snapshot_id == *snapshot_id
            && value.request_hash == request_hash(request)?
            && value.saved_pins_hash == ObjectHash::body(&request.payload.saved_pins)?
            && value.selections.len() <= 1
            && value.source_node_public_key == *local
            && request.payload.issued_at_ms <= value.issued_at_ms
            && value.issued_at_ms <= now
            && value.expires_at_ms == request.payload.expires_at_ms
            && now < value.expires_at_ms,
        "client policy selections page changed"
    );
    page.verify_signature()?;
    let mut usage = signed_usage(page)?;
    for selection in &value.selections {
        let verified = verify_client_policy_read_selection(
            policy.verified(),
            request,
            selection,
            local,
            &PublicKey::parse(principal)?,
            now,
        )?;
        check_heads_in(connection, policy, verified.heads())?;
        usage.merge(verified.proof_usage());
    }
    Ok(usage)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::identity::test_support::{TestNetwork, now};
    use vibeface_secure::{
        authorization::{
            Audience, DeviceId, JournalEntry, ResourceId, ServiceId,
            bootstrap::{
                self,
                recovery::{self, node_enrollment},
            },
        },
        signature_verification::SignatureVerification,
    };

    fn selection(network: &TestNetwork) -> Result<AuthorizationSelection> {
        let node = PublicKey::parse(&network.node.public_key_hex()?)?;
        Ok(AuthorizationSelection {
            resource: ResourceRef {
                space: network.space.clone(),
                resource: network.resource.clone(),
            },
            audience: Audience {
                service: ServiceId::from_key(&node),
                device: DeviceId::from_key(&node),
            },
            grant_leaf: network.bundle.grant_leaf.clone(),
        })
    }

    fn read_request(
        network: &TestNetwork,
        selected: bool,
    ) -> Result<Signed<ClientPolicyReadRequest>> {
        Ok(Signed::sign(
            ClientPolicyReadRequest {
                version: AUTH_VERSION,
                account: network.account.clone(),
                space: network.space.clone(),
                device_public_key: PublicKey::of(&network.client),
                node_public_key: PublicKey::parse(&network.node.public_key_hex()?)?,
                saved_pins: network.pins.clone(),
                selection: if selected {
                    Some(selection(network)?)
                } else {
                    None
                },
                nonce: Nonce::random()?,
                issued_at_ms: now(),
                expires_at_ms: now() + 90_000,
            },
            &network.client,
        )?)
    }

    fn measured(label: &str, action: impl FnOnce() -> Result<Value>) -> Result<Value> {
        let verification = SignatureVerification::new();
        let response = verification.run(action)?;
        let counts = verification.counts();
        let bytes = serde_json::to_vec(&response)?.len();
        assert!(counts.actual_verifications <= 32);
        assert!(bytes <= MAX_PROOF_BYTES);
        eprintln!(
            "client_policy {label}: bytes={bytes}, actual={}, hits={}",
            counts.actual_verifications, counts.cache_hits
        );
        Ok(response)
    }

    fn begin(
        store: &mut Store,
        network: &TestNetwork,
        request: Signed<ClientPolicyReadRequest>,
    ) -> Result<Signed<ClientPolicySnapshot>> {
        let response = measured("begin", || {
            store.begin_client_policy_read(
                &network.node,
                &network.client.public_key_hex(),
                request,
                now(),
            )
        })?;
        assert!(response.get("authorization").is_none());
        Ok(serde_json::from_value(response["snapshot"].clone())?)
    }

    fn page_request(
        snapshot: &Signed<ClientPolicySnapshot>,
        cursor: Option<ObjectHash>,
    ) -> Result<Value> {
        Ok(serde_json::to_value(ClientPolicyPageRequest {
            snapshot_id: snapshot.payload.snapshot_id.clone(),
            request_hash: snapshot.payload.request_hash.clone(),
            cursor,
        })?)
    }

    fn import_pages(
        store: &mut Store,
        network: &TestNetwork,
        snapshot: &Signed<ClientPolicySnapshot>,
        mut native: Option<&mut ClientPolicyImport>,
    ) -> Result<(Vec<JournalEntry>, Signed<ClientPolicyWitness>)> {
        let mut cursor = None;
        let mut entries = Vec::new();
        let mut page_count = 0;
        loop {
            let params = page_request(snapshot, cursor)?;
            if let Some(native) = native.as_deref_mut() {
                assert_eq!(
                    serde_json::from_str::<Value>(
                        &native.page_request_json().map_err(anyhow::Error::msg)?
                    )?,
                    params
                );
            }
            let response = measured("page", || {
                store.client_policy_page(
                    &network.node,
                    &network.client.public_key_hex(),
                    &params,
                    now(),
                )
            })?;
            let replay = measured("page_retry", || {
                store.client_policy_page(
                    &network.node,
                    &network.client.public_key_hex(),
                    &params,
                    now(),
                )
            })?;
            assert_eq!(response, replay);
            let page: Signed<ClientPolicyPage> = serde_json::from_value(response["page"].clone())?;
            if let Some(native) = native.as_deref_mut() {
                let encoded = serde_json::to_string(&page)?;
                assert!(encoded.len() <= MAX_PROOF_BYTES);
                measured_native("append_page", || native.append_page(&encoded, now() as f64))?;
            }
            page.verify_signature()?;
            assert_eq!(page.payload.page_index, page_count);
            page_count += 1;
            entries.extend(page.payload.entry);
            if page.payload.complete {
                break;
            }
            cursor = page.payload.next_cursor;
            assert!(cursor.is_some());
            assert!(page_count < 4096);
        }
        let params = page_request(snapshot, None)?;
        let response = measured("witness", || {
            store.client_policy_witness(
                &network.node,
                &network.client.public_key_hex(),
                &params,
                now(),
            )
        })?;
        let witness: Signed<ClientPolicyWitness> =
            serde_json::from_value(response["witness"].clone())?;
        assert_eq!(witness.payload.entry_count, entries.len() as u64);
        assert_eq!(witness.payload.page_count, page_count);
        Ok((entries, witness))
    }

    fn measured_native<T>(
        label: &str,
        action: impl FnOnce() -> std::result::Result<T, String>,
    ) -> Result<T> {
        let verification = SignatureVerification::new();
        let value = verification.run(action).map_err(anyhow::Error::msg)?;
        let counts = verification.counts();
        assert!(counts.actual_verifications <= 32);
        eprintln!(
            "client_policy native_{label}: actual={}, hits={}",
            counts.actual_verifications, counts.cache_hits
        );
        Ok(value)
    }

    fn confirmation_request(
        network: &TestNetwork,
        snapshot: &Signed<ClientPolicySnapshot>,
        witness: &Signed<ClientPolicyWitness>,
    ) -> Result<Signed<ClientPolicyAuthConfirmRequest>> {
        Ok(Signed::sign(
            ClientPolicyAuthConfirmRequest {
                version: AUTH_VERSION,
                context: ClientPolicyContextReference {
                    snapshot_id: snapshot.payload.snapshot_id.clone(),
                    request_hash: snapshot.payload.request_hash.clone(),
                    snapshot_hash: snapshot.hash()?,
                    witness_hash: witness.hash()?,
                },
                selection: snapshot.payload.selection.clone(),
                target_heads: snapshot.payload.target_heads.clone(),
                device_public_key: PublicKey::of(&network.client),
                issued_at_ms: now(),
                expires_at_ms: now() + 90_000,
            },
            &network.client,
        )?)
    }

    #[test]
    fn client_policy_read_pages_confirm_and_retry_without_authorization_bundle() -> Result<()> {
        let network = TestNetwork::new()?;
        let mut store = network.memory()?;
        let request = read_request(&network, true)?;
        let snapshot = begin(&mut store, &network, request.clone())?;
        let repeated = begin(&mut store, &network, request.clone())?;
        assert_eq!(snapshot, repeated);
        assert!(
            store
                .client_policy_witness(
                    &network.node,
                    &network.client.public_key_hex(),
                    &page_request(&snapshot, None)?,
                    now()
                )
                .is_err()
        );
        let (entries, witness) = import_pages(&mut store, &network, &snapshot, None)?;
        assert_eq!(entries, network.bundle.journal);
        let request = confirmation_request(&network, &snapshot, &witness)?;
        let params = serde_json::to_value(&request)?;
        store.connection.execute_batch("CREATE TEMP TRIGGER fail_client_confirmation BEFORE INSERT ON identity_meta WHEN NEW.key GLOB 'client-policy-confirmation:*' BEGIN SELECT RAISE(ABORT,'expected confirmation failure'); END;")?;
        assert!(
            store
                .confirm_client_policy_auth(
                    &network.node,
                    &network.client.public_key_hex(),
                    &params,
                    now()
                )
                .is_err()
        );
        assert!(
            metadata(
                &store.connection,
                &confirmation_key(&snapshot.payload.snapshot_id)
            )?
            .is_none()
        );
        store
            .connection
            .execute_batch("DROP TRIGGER fail_client_confirmation")?;
        let response = measured("confirm", || {
            store.confirm_client_policy_auth(
                &network.node,
                &network.client.public_key_hex(),
                &params,
                now(),
            )
        })?;
        let confirmation: Signed<ClientPolicyAuthConfirmation> =
            serde_json::from_value(response["confirmation"].clone())?;
        assert_eq!(
            confirmation.payload.confirmation_request_hash,
            request.hash()?
        );
        confirmation.verify_signature()?;
        assert_eq!(
            response,
            measured("confirm_retry", || store.confirm_client_policy_auth(
                &network.node,
                &network.client.public_key_hex(),
                &params,
                now()
            ))?
        );
        let count: usize = store.connection.query_row(
            "SELECT count(*) FROM identity_meta WHERE key GLOB 'client-policy-confirmation:*'",
            [],
            |row| row.get(0),
        )?;
        assert_eq!(count, 1);
        let mut changed = request.payload;
        changed.expires_at_ms -= 1;
        assert!(
            store
                .confirm_client_policy_auth(
                    &network.node,
                    &network.client.public_key_hex(),
                    &serde_json::to_value(Signed::sign(changed, &network.client)?)?,
                    now()
                )
                .is_err()
        );
        Ok(())
    }

    #[test]
    fn client_policy_read_unselected_catalog_returns_only_verified_bounded_choices() -> Result<()> {
        let network = TestNetwork::new()?;
        let mut store = network.memory()?;
        let request = read_request(&network, false)?;
        let response = measured("choices_begin", || {
            store.begin_client_policy_read(
                &network.node,
                &network.client.public_key_hex(),
                request.clone(),
                now(),
            )
        })?;
        assert!(response.get("snapshot").is_none());
        assert!(response.get("authorization").is_none());
        let first = response.clone();
        let mut page: Signed<ClientPolicySelectionsPage> =
            serde_json::from_value(response["selections"].clone())?;
        let mut choices = Vec::new();
        loop {
            assert!(page.payload.selections.len() <= 1);
            choices.extend(page.payload.selections);
            let Some(cursor) = page.payload.next_cursor else {
                break;
            };
            let params = serde_json::to_value(ClientPolicyPageRequest {
                snapshot_id: page.payload.snapshot_id,
                request_hash: page.payload.request_hash,
                cursor: Some(cursor),
            })?;
            let response = measured("choices_next", || {
                store.client_policy_selections(
                    &network.node,
                    &network.client.public_key_hex(),
                    &params,
                    now(),
                )
            })?;
            assert_eq!(
                response,
                measured("choices_retry", || store.client_policy_selections(
                    &network.node,
                    &network.client.public_key_hex(),
                    &params,
                    now()
                ))?
            );
            page = serde_json::from_value(response["selections"].clone())?;
        }
        assert!(choices.contains(&selection(&network)?));
        assert_eq!(
            first,
            measured("choices_begin_retry", || store.begin_client_policy_read(
                &network.node,
                &network.client.public_key_hex(),
                request,
                now()
            ))?
        );
        let selected = read_request(&network, true)?;
        begin(&mut store, &network, selected)?;
        Ok(())
    }

    #[test]
    fn client_policy_read_rejects_noise_scope_nonce_cursor_and_expiry_changes() -> Result<()> {
        let network = TestNetwork::new()?;
        let mut store = network.memory()?;
        let request = read_request(&network, true)?;
        let foreign = Identity::generate().map_err(anyhow::Error::msg)?;
        assert!(
            store
                .begin_client_policy_read(
                    &network.node,
                    &foreign.public_key_hex(),
                    request.clone(),
                    now()
                )
                .is_err()
        );
        let snapshot = begin(&mut store, &network, request.clone())?;
        let mut changed = request.payload.clone();
        changed.expires_at_ms -= 1;
        assert!(
            store
                .begin_client_policy_read(
                    &network.node,
                    &network.client.public_key_hex(),
                    Signed::sign(changed, &network.client)?,
                    now()
                )
                .is_err()
        );
        assert!(
            store
                .client_policy_page(
                    &network.node,
                    &network.client.public_key_hex(),
                    &page_request(&snapshot, Some(ObjectHash::body(&"other cursor")?))?,
                    now()
                )
                .is_err()
        );
        assert!(
            store
                .client_policy_page(
                    &network.node,
                    &foreign.public_key_hex(),
                    &page_request(&snapshot, None)?,
                    now()
                )
                .is_err()
        );
        assert!(
            store
                .client_policy_page(
                    &network.node,
                    &network.client.public_key_hex(),
                    &page_request(&snapshot, None)?,
                    request.payload.expires_at_ms
                )
                .is_err()
        );
        let mut changed = read_request(&network, true)?.payload;
        changed.selection.as_mut().unwrap().resource.resource = ResourceId::random()?;
        assert!(
            store
                .begin_client_policy_read(
                    &network.node,
                    &network.client.public_key_hex(),
                    Signed::sign(changed, &network.client)?,
                    now()
                )
                .is_err()
        );
        Ok(())
    }

    #[test]
    fn client_policy_read_rejects_policy_change_before_page_and_confirm_on_another_connection()
    -> Result<()> {
        let network = TestNetwork::new()?;
        let path = network.directory.join("bounded-client.sqlite3");
        let mut store = network.at(&path)?;
        let mut writer = Store::open(&path)?;
        let snapshot = begin(&mut store, &network, read_request(&network, true)?)?;
        let (_, witness) = import_pages(&mut store, &network, &snapshot, None)?;
        let confirm = serde_json::to_value(confirmation_request(&network, &snapshot, &witness)?)?;
        network.revoke_client(&mut writer)?;
        assert!(
            store
                .client_policy_page(
                    &network.node,
                    &network.client.public_key_hex(),
                    &page_request(&snapshot, None)?,
                    now() + 2
                )
                .is_err()
        );
        assert!(
            store
                .confirm_client_policy_auth(
                    &network.node,
                    &network.client.public_key_hex(),
                    &confirm,
                    now() + 2
                )
                .is_err()
        );
        assert!(
            store
                .begin_client_policy_read(
                    &network.node,
                    &network.client.public_key_hex(),
                    read_request(&network, true)?,
                    now() + 2
                )
                .is_err()
        );
        let count: usize = store.connection.query_row(
            "SELECT count(*) FROM identity_meta WHERE key GLOB 'client-policy-confirmation:*'",
            [],
            |row| row.get(0),
        )?;
        assert_eq!(count, 0);
        Ok(())
    }

    #[test]
    fn client_policy_read_rejects_changed_pin_and_private_snapshot_without_advancing() -> Result<()>
    {
        let network = TestNetwork::new()?;
        let mut store = network.memory()?;
        let snapshot = begin(&mut store, &network, read_request(&network, true)?)?;
        let key = context_key(&snapshot.payload.snapshot_id);
        let original = metadata(&store.connection, &key)?.context("snapshot missing")?;
        let mut changed: ClientReadRecord = serde_json::from_str(&original)?;
        changed.snapshot_salt = Nonce::random()?;
        store.connection.execute(
            "UPDATE identity_meta SET value=? WHERE key=?",
            params![encode(&changed)?, key],
        )?;
        let params = page_request(&snapshot, None)?;
        assert!(
            store
                .client_policy_page(
                    &network.node,
                    &network.client.public_key_hex(),
                    &params,
                    now()
                )
                .is_err()
        );
        assert!(metadata(&store.connection, &page_key(&snapshot.payload.snapshot_id))?.is_none());
        store.connection.execute(
            "UPDATE identity_meta SET value=? WHERE key=?",
            params![original, key],
        )?;
        let root = network
            .pins
            .iter()
            .find(|pin| pin.domain == PolicyDomain::Account(network.account.clone()))
            .context("account pin missing")?
            .genesis_root
            .clone();
        let other = Identity::generate().map_err(anyhow::Error::msg)?;
        store.connection.execute(
            "UPDATE policy_pins SET genesis_root=? WHERE domain_kind='account' AND domain_id=?",
            params![other.public_key_hex(), network.account.as_str()],
        )?;
        assert!(
            store
                .client_policy_page(
                    &network.node,
                    &network.client.public_key_hex(),
                    &params,
                    now()
                )
                .is_err()
        );
        assert!(metadata(&store.connection, &page_key(&snapshot.payload.snapshot_id))?.is_none());
        store.connection.execute(
            "UPDATE policy_pins SET genesis_root=? WHERE domain_kind='account' AND domain_id=?",
            params![root.as_str(), network.account.as_str()],
        )?;
        measured("page_after_rejected_tamper", || {
            store.client_policy_page(
                &network.node,
                &network.client.public_key_hex(),
                &params,
                now(),
            )
        })?;
        Ok(())
    }

    fn with_twelve_enrolled_nodes()
    -> Result<(TestNetwork, Vec<vibeface_secure::authorization::PolicyPin>)> {
        let mut network = TestNetwork::new()?;
        let saved = network.pins.clone();
        let material = recovery::build_account_recovery_material(
            &network.created,
            &network.root,
            &network.recovery,
            "https://node.example",
            now() as f64,
        )
        .map_err(anyhow::Error::msg)?;
        let authority = signer(&network.node)?;
        for _ in 0..12 {
            let node = Identity::generate().map_err(anyhow::Error::msg)?;
            let nonce = Nonce::random()?;
            let claim = bootstrap::create_node_claim(
                &node,
                nonce.as_str(),
                now() as f64,
                (now() + 90_000) as f64,
            )
            .map_err(anyhow::Error::msg)?;
            let claim = bootstrap::verify_node_claim(
                &claim,
                &node.public_key_hex(),
                nonce.as_str(),
                now() as f64,
            )
            .map_err(anyhow::Error::msg)?;
            let pins = serde_json::to_string(&network.pins)?;
            let bundle = serde_json::to_string(&network.bundle)?;
            let prepared = node_enrollment::prepare_account_node_enrollment(
                &material,
                &pins,
                &bundle,
                &network.client,
                &claim,
                (now() + 1_000_000) as f64,
                now() as f64,
            )
            .map_err(anyhow::Error::msg)?;
            let proof = node_enrollment::sign_account_node_enrollment_possession(
                &prepared.request_json().map_err(anyhow::Error::msg)?,
                &node,
                nonce.as_str(),
                now() as f64,
            )
            .map_err(anyhow::Error::msg)?;
            let prepared = node_enrollment::prepare_account_node_enrollment_commit(
                &prepared,
                &material,
                &proof,
                now() as f64,
            )
            .map_err(anyhow::Error::msg)?;
            let delta = node_enrollment::commit_account_node_enrollment(
                &prepared.request_json().map_err(anyhow::Error::msg)?,
                &authority,
                &pins,
                &bundle,
                now() as f64,
            )
            .map_err(anyhow::Error::msg)?;
            let finished =
                node_enrollment::finish_account_node_enrollment(&prepared, &delta, now() as f64)
                    .map_err(anyhow::Error::msg)?;
            network.bundle = finished.bundle().clone();
            network.pins = finished.pins().to_vec();
        }
        Ok((network, saved))
    }

    #[test]
    fn client_policy_read_normal_twelve_node_login_keeps_full_public_history_within_each_carrier()
    -> Result<()> {
        let (network, saved) = with_twelve_enrolled_nodes()?;
        let public_bytes = serde_json::to_vec(&network.bundle)?.len();
        assert!(public_bytes > MAX_PROOF_BYTES);
        assert_eq!(
            network
                .bundle
                .journal
                .iter()
                .filter(|entry| matches!(entry, JournalEntry::AccountCommit(_)))
                .count(),
            12
        );
        let mut store = network.memory()?;
        let mut request = read_request(&network, true)?.payload;
        request.saved_pins = saved;
        let request = Signed::sign(request, &network.client)?;
        let snapshot = begin(&mut store, &network, request.clone())?;
        let pins = serde_json::to_string(&request.payload.saved_pins)?;
        let encoded_request = serde_json::to_string(&request)?;
        let encoded_snapshot = serde_json::to_string(&snapshot)?;
        let node = network.node.public_key_hex()?;
        let mut native = measured_native("begin_import", || {
            begin_client_policy_import(
                &pins,
                &encoded_request,
                &encoded_snapshot,
                &network.client,
                &node,
                now() as f64,
            )
        })?;
        let (entries, witness) = import_pages(&mut store, &network, &snapshot, Some(&mut native))?;
        assert_eq!(entries, network.bundle.journal);
        let encoded_witness = serde_json::to_string(&witness)?;
        let encoded_selection = serde_json::to_string(&snapshot.payload.selection)?;
        let authorization = measured_native("finish_import", || {
            native.finish(&encoded_witness, &encoded_selection, now() as f64)
        })?;
        assert_eq!(authorization.scope().account_id, network.account);
        assert_eq!(authorization.resources().len(), 1);
        assert_eq!(
            authorization.resources()[0].payload.resource,
            network.resource
        );
        assert!(
            authorization
                .policy_json()
                .map_err(anyhow::Error::msg)?
                .len()
                <= MAX_PROOF_BYTES
        );
        let confirm_request = measured_native("build_confirm", || {
            build_client_policy_auth_confirm_request(
                &authorization,
                &network.client,
                now() as f64,
                (now() + 90_000) as f64,
            )
        })?;
        let params: Value = serde_json::from_str(&confirm_request)?;
        let response = measured("12_node_confirm", || {
            store.confirm_client_policy_auth(
                &network.node,
                &network.client.public_key_hex(),
                &params,
                now(),
            )
        })?;
        let encoded_confirmation = serde_json::to_string(&response["confirmation"])?;
        let authorization = measured_native("verify_confirmation", || {
            confirm_client_policy_authorization(
                &authorization,
                &confirm_request,
                &encoded_confirmation,
                &node,
                now() as f64,
            )
        })?;
        assert_eq!(authorization.grant_leaf(), &network.bundle.grant_leaf);
        eprintln!(
            "client_policy twelve_node_public_history_bytes={public_bytes}; entries={}",
            entries.len()
        );
        Ok(())
    }
}
