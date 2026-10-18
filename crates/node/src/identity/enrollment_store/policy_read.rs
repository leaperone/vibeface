use super::*;
use crate::identity::{
    client_policy_read::check_heads_in,
    journal::{AcceptedJournalCursor, AcceptedJournalSnapshot, PolicyState},
};
use std::collections::BTreeSet;
use vibeface_secure::{
    Identity,
    authorization::{
        MAX_PROOF_BYTES,
        bootstrap::recovery::node_enrollment::{
            AccountNodeEnrollmentCredential, verify_account_node_enrollment_read,
        },
        enrollment_policy_import::*,
        execution_handoff::ExecutionProofUsage,
        policy_proof::policy_entry_payload_usage,
    },
    signature_verification::{ScopedSignatureVerification, SignatureVerification},
};

#[derive(Clone, Serialize, Deserialize)]
#[serde(
    tag = "purpose",
    content = "credential",
    rename_all = "snake_case",
    deny_unknown_fields
)]
#[allow(clippy::large_enum_variant)]
enum ReadCredential {
    BrowserDevice(Signed<EnrollmentOffer>),
    AccountNode(AccountNodeEnrollmentCredential),
}
impl ReadCredential {
    fn descriptor(&self) -> EnrollmentPolicyDescriptor {
        match self {
            Self::BrowserDevice(offer) => offer::enrollment_offer_policy_descriptor(offer),
            Self::AccountNode(credential) => credential.descriptor.clone(),
        }
    }
    fn hash(&self) -> Result<ObjectHash> {
        Ok(match self {
            Self::BrowserDevice(offer) => offer::enrollment_offer_hash(offer)?,
            Self::AccountNode(credential) => ObjectHash::body(credential)?,
        })
    }
    fn verify(
        &self,
        connection: &Connection,
        policy: &PolicyState,
        request: &Signed<EnrollmentPolicyReadRequest>,
        local: &PublicKey,
        noise: &PublicKey,
        now: u64,
    ) -> Result<ExecutionProofUsage> {
        let descriptor = self.descriptor();
        let mut usage = match self {
            Self::BrowserDevice(offer) => {
                let published: String = connection.query_row(
                    "SELECT value FROM local_values WHERE space_id=? AND key=?",
                    params![
                        SpaceId::personal(&offer.payload.account).as_str(),
                        offer_key(&self.hash()?)
                    ],
                    |row| row.get(0),
                )?;
                ensure!(
                    serde_json::from_str::<Signed<EnrollmentOffer>>(&published)? == *offer,
                    "enrollment read changed its published QR credential"
                );
                offer::verify_enrollment_offer_policy_read(
                    policy.verified(),
                    offer,
                    request,
                    local,
                    noise,
                    now,
                )?
            }
            Self::AccountNode(credential) => {
                let mut usage = verify_account_node_enrollment_read(
                    policy.verified(),
                    &credential.intent,
                    &credential.descriptor,
                    local,
                    noise,
                    now,
                )?;
                usage.merge(&verify_enrollment_policy_read_request(
                    request,
                    &descriptor,
                    &self.hash()?,
                    local,
                    noise,
                    now,
                )?);
                ensure!(
                    request.payload.target_public_key
                        == credential.intent.payload.claim.payload.node_public_key,
                    "node enrollment read changed its approved target"
                );
                usage
            }
        };
        usage.record_metadata(self)?;
        check_heads_in(connection, policy, &descriptor.target_heads)?;
        if let Some(counts) = SignatureVerification::current_counts() {
            usage.record_actual_signature_counts(counts);
        }
        usage.enforce_limits()?;
        Ok(usage)
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct BeginRequest {
    credential: ReadCredential,
    request: Signed<EnrollmentPolicyReadRequest>,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ReadRecord {
    credential: ReadCredential,
    request: Signed<EnrollmentPolicyReadRequest>,
    snapshot: Signed<EnrollmentPolicySnapshot>,
    journal_snapshot: AcceptedJournalSnapshot,
    salt: Nonce,
    position: ReadPosition,
    witness: Option<Signed<EnrollmentPolicyWitness>>,
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
    page: Signed<EnrollmentPolicyPage>,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RequestBinding {
    request_hash: ObjectHash,
    snapshot_id: Nonce,
}

fn context_key(id: &Nonce) -> String {
    format!("enrollment-policy-read:{id}")
}
fn page_key(id: &Nonce) -> String {
    format!("enrollment-policy-page:{id}")
}
fn request_key(request: &Signed<EnrollmentPolicyReadRequest>) -> String {
    format!(
        "enrollment-policy-request:{}:{}",
        request.payload.reader_public_key, request.payload.nonce
    )
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
        length.is_none_or(|length| length <= MAX_PROOF_BYTES * 3),
        "stored enrollment policy context too large"
    );
    Ok(connection
        .query_row(
            "SELECT value FROM identity_meta WHERE key=?",
            [key],
            |row| row.get(0),
        )
        .optional()?)
}
fn encoded<T: Serialize>(value: &T) -> Result<String> {
    let encoded = serde_json::to_string(value)?;
    ensure!(
        encoded.len() <= MAX_PROOF_BYTES * 3,
        "stored enrollment policy context too large"
    );
    Ok(encoded)
}
fn bounded(value: &Value) -> Result<()> {
    ensure!(
        serde_json::to_vec(value)?.len() <= MAX_PROOF_BYTES,
        "enrollment policy carrier exceeds 64 KiB"
    );
    Ok(())
}
fn response(value: Value, mut usage: ExecutionProofUsage, input: &Value) -> Result<Value> {
    usage.record_metadata(input)?;
    ensure!(
        serde_json::to_vec(input)?
            .len()
            .checked_add(serde_json::to_vec(&value)?.len())
            .is_some_and(|bytes| bytes <= MAX_PROOF_BYTES),
        "enrollment policy complete carrier exceeds 64 KiB"
    );
    if let Some(counts) = SignatureVerification::current_counts() {
        usage.record_actual_signature_counts(counts);
    }
    usage.enforce_limits()?;
    bounded(&value)?;
    #[cfg(test)]
    eprintln!("enrollment_policy proof_usage={:?}", usage.counts());
    Ok(value)
}
fn snapshot_id(
    request: &ObjectHash,
    journal: &AcceptedJournalSnapshot,
    salt: &Nonce,
) -> Result<Nonce> {
    let hash = ObjectHash::body(&("enrollment-policy-snapshot-v2", request, journal, salt))?;
    Ok(Nonce::parse(
        hash.as_str()
            .strip_prefix("h2_")
            .context("invalid enrollment snapshot hash")?,
    )?)
}
fn cursor(record: &ReadRecord, prior_hash: &ObjectHash) -> Result<ObjectHash> {
    Ok(ObjectHash::body(&(
        "enrollment-policy-cursor-v2",
        record.snapshot.hash()?,
        &record.salt,
        &record.position.journal_cursor,
        record.position.page_count,
        record.position.entry_count,
        prior_hash,
    ))?)
}
fn verify_record(
    connection: &Connection,
    policy: &PolicyState,
    record: &ReadRecord,
    local: &PublicKey,
    noise: &PublicKey,
    now: u64,
) -> Result<(Option<CachedPage>, ExecutionProofUsage)> {
    let mut usage =
        record
            .credential
            .verify(connection, policy, &record.request, local, noise, now)?;
    let descriptor = record.credential.descriptor();
    let snapshot = &record.snapshot.payload;
    record.snapshot.verify_signature()?;
    usage.record_signed(&record.snapshot)?;
    ensure!(
        record.snapshot.issuer == *local
            && snapshot.version == AUTH_VERSION
            && snapshot.purpose == descriptor.purpose
            && snapshot.credential_hash == record.credential.hash()?
            && snapshot.request_hash == record.request.hash()?
            && snapshot.descriptor_hash == ObjectHash::body(&descriptor)?
            && snapshot.snapshot_id
                == snapshot_id(
                    &record.request.hash()?,
                    &record.journal_snapshot,
                    &record.salt
                )?
            && snapshot.reader_public_key == record.request.payload.reader_public_key
            && snapshot.target_public_key == record.request.payload.target_public_key
            && snapshot.source_node_public_key == *local
            && snapshot.saved_pins_hash == ObjectHash::body(&descriptor.pins)?
            && snapshot.required_heads_hash == ObjectHash::body(&descriptor.required_heads)?
            && snapshot.target_heads == descriptor.target_heads
            && record.request.payload.issued_at_ms <= snapshot.issued_at_ms
            && snapshot.issued_at_ms <= now
            && snapshot.expires_at_ms == record.request.payload.expires_at_ms
            && now < snapshot.expires_at_ms,
        "stored enrollment policy snapshot changed its signed binding"
    );
    let cached = metadata(connection, &page_key(&snapshot.snapshot_id))?
        .map(|value| serde_json::from_str::<CachedPage>(&value))
        .transpose()?;
    if let Some(cached) = &cached {
        let page = &cached.page.payload;
        cached.page.verify_signature()?;
        usage.record_signed(&cached.page)?;
        if let Some(entry) = &page.entry {
            usage.merge(&policy_entry_payload_usage(entry)?);
        }
        ensure!(
            cached.page.issuer == *local
                && page.version == AUTH_VERSION
                && page.purpose == snapshot.purpose
                && page.credential_hash == snapshot.credential_hash
                && page.request_hash == snapshot.request_hash
                && page.snapshot_id == snapshot.snapshot_id
                && page.snapshot_hash == record.snapshot.hash()?
                && page.page_index.checked_add(1) == Some(record.position.page_count)
                && cached.page.hash()? == record.position.previous_page_hash
                && page.next_cursor == record.position.cursor
                && page.complete == record.witness.is_some()
                && page.expires_at_ms == snapshot.expires_at_ms
                && page.issued_at_ms <= now
                && (!page.complete || page.next_cursor.is_none())
                && (page.complete
                    || page.next_cursor.as_ref()
                        == Some(&cursor(record, &page.previous_page_hash)?)),
            "stored enrollment policy cursor changed its last signed page"
        );
    } else {
        ensure!(
            record.position.page_count == 0
                && record.position.entry_count == 0
                && record.position.previous_page_hash == record.snapshot.hash()?
                && record.position.cursor.is_none()
                && record.position.journal_cursor.is_none()
                && record.witness.is_none(),
            "stored enrollment policy position has no signed page"
        );
    }
    if let Some(witness) = &record.witness {
        witness.verify_signature()?;
        usage.record_signed(witness)?;
        let value = &witness.payload;
        ensure!(
            witness.issuer == *local
                && value.version == AUTH_VERSION
                && value.purpose == snapshot.purpose
                && value.credential_hash == snapshot.credential_hash
                && value.request_hash == snapshot.request_hash
                && value.snapshot_id == snapshot.snapshot_id
                && value.snapshot_hash == record.snapshot.hash()?
                && value.target_heads_hash == ObjectHash::body(&snapshot.target_heads)?
                && value.final_page_hash == record.position.previous_page_hash
                && value.page_count == record.position.page_count
                && value.entry_count == record.position.entry_count
                && value.issued_at_ms <= now
                && value.expires_at_ms == snapshot.expires_at_ms,
            "stored enrollment policy witness changed its completion"
        );
    }
    Ok((cached, usage))
}

impl Store {
    pub(crate) fn begin_enrollment_policy_read(
        &mut self,
        identity: &LocalIdentity,
        principal: &str,
        params: &Value,
        now: u64,
    ) -> Result<Value> {
        let _scope = ScopedSignatureVerification::new();
        bounded(params)?;
        let input: BeginRequest = serde_json::from_value(params.clone())?;
        let signer =
            Identity::from_secret_hex(&identity.secret_key_hex).map_err(anyhow::Error::msg)?;
        let local = PublicKey::of(&signer);
        let noise = PublicKey::parse(principal)?;
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut usage =
            input
                .credential
                .verify(&tx, &self.policy, &input.request, &local, &noise, now)?;
        let binding_key = request_key(&input.request);
        if let Some(binding) = metadata(&tx, &binding_key)? {
            let binding: RequestBinding = serde_json::from_str(&binding)?;
            ensure!(
                binding.request_hash == input.request.hash()?,
                "enrollment policy read nonce reused"
            );
            let record: ReadRecord = serde_json::from_str(
                &metadata(&tx, &context_key(&binding.snapshot_id))?
                    .context("enrollment snapshot missing")?,
            )?;
            ensure!(
                record.credential.hash()? == input.credential.hash()?
                    && record.request == input.request,
                "enrollment policy read retry changed original credential"
            );
            let (_, recorded_usage) =
                verify_record(&tx, &self.policy, &record, &local, &noise, now)?;
            usage.merge(&recorded_usage);
            return response(json!({"snapshot":record.snapshot}), usage, params);
        }
        let descriptor = input.credential.descriptor();
        let journal_snapshot = self.policy.accepted_journal_snapshot(&tx)?;
        let salt = Nonce::random()?;
        let id = snapshot_id(&input.request.hash()?, &journal_snapshot, &salt)?;
        let snapshot = Signed::sign(
            EnrollmentPolicySnapshot {
                version: AUTH_VERSION,
                purpose: descriptor.purpose,
                credential_hash: input.credential.hash()?,
                request_hash: input.request.hash()?,
                snapshot_id: id.clone(),
                descriptor_hash: ObjectHash::body(&descriptor)?,
                reader_public_key: input.request.payload.reader_public_key.clone(),
                target_public_key: input.request.payload.target_public_key.clone(),
                source_node_public_key: local,
                saved_pins_hash: ObjectHash::body(&descriptor.pins)?,
                required_heads_hash: ObjectHash::body(&descriptor.required_heads)?,
                target_heads: descriptor.target_heads,
                issued_at_ms: now,
                expires_at_ms: input.request.payload.expires_at_ms,
            },
            &signer,
        )?;
        usage.record_signed(&snapshot)?;
        let result = response(json!({"snapshot":snapshot}), usage, params)?;
        let previous_page_hash = snapshot.hash()?;
        let record = ReadRecord {
            credential: input.credential,
            request: input.request,
            snapshot,
            journal_snapshot,
            salt,
            position: ReadPosition {
                cursor: None,
                journal_cursor: None,
                page_count: 0,
                entry_count: 0,
                previous_page_hash,
            },
            witness: None,
        };
        tx.execute(
            "INSERT INTO identity_meta(key,value) VALUES(?,?)",
            params![context_key(&id), encoded(&record)?],
        )?;
        tx.execute(
            "INSERT INTO identity_meta(key,value) VALUES(?,?)",
            params![
                binding_key,
                encoded(&RequestBinding {
                    request_hash: record.request.hash()?,
                    snapshot_id: id
                })?
            ],
        )?;
        tx.commit()?;
        Ok(result)
    }

    pub(crate) fn enrollment_policy_page(
        &mut self,
        identity: &LocalIdentity,
        principal: &str,
        params: &Value,
        now: u64,
    ) -> Result<Value> {
        let _scope = ScopedSignatureVerification::new();
        bounded(params)?;
        let request: EnrollmentPolicyPageRequest = serde_json::from_value(params.clone())?;
        let signer =
            Identity::from_secret_hex(&identity.secret_key_hex).map_err(anyhow::Error::msg)?;
        let local = PublicKey::of(&signer);
        let noise = PublicKey::parse(principal)?;
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let key = context_key(&request.snapshot_id);
        let original = metadata(&tx, &key)?.context("enrollment policy read is missing")?;
        let mut record: ReadRecord = serde_json::from_str(&original)?;
        ensure!(
            record.snapshot.payload.snapshot_id == request.snapshot_id
                && record.request.hash()? == request.request_hash,
            "enrollment policy page request changed"
        );
        let (cached, mut usage) = verify_record(&tx, &self.policy, &record, &local, &noise, now)?;
        if let Some(cached) = cached
            && cached.page.payload.request_cursor == request.cursor
        {
            return response(json!({"page":cached.page}), usage, params);
        }
        ensure!(
            record.witness.is_none() && request.cursor == record.position.cursor,
            "enrollment policy cursor is stale or complete"
        );
        let domains: BTreeSet<_> = record
            .credential
            .descriptor()
            .pins
            .iter()
            .map(|pin| pin.domain.clone())
            .collect();
        let scanned = self.policy.accepted_journal_page(
            &tx,
            &record.journal_snapshot,
            record.position.journal_cursor.as_ref(),
            &domains,
        )?;
        ensure!(
            scanned.scanned <= 32,
            "enrollment policy scan budget exceeded"
        );
        let entry = scanned.entry.map(|value| value.entry);
        if let Some(entry) = &entry {
            usage.merge(&policy_entry_payload_usage(entry)?);
        }
        let previous_page_hash = record.position.previous_page_hash.clone();
        let page_index = record.position.page_count;
        record.position.page_count = page_index
            .checked_add(1)
            .context("enrollment policy page overflow")?;
        record.position.entry_count = record
            .position
            .entry_count
            .checked_add(u64::from(entry.is_some()))
            .context("enrollment policy entry overflow")?;
        record.position.journal_cursor = Some(scanned.next_cursor);
        let next_cursor = if scanned.complete {
            None
        } else {
            Some(cursor(&record, &previous_page_hash)?)
        };
        let snapshot = &record.snapshot.payload;
        let page = Signed::sign(
            EnrollmentPolicyPage {
                version: AUTH_VERSION,
                purpose: snapshot.purpose,
                credential_hash: snapshot.credential_hash.clone(),
                request_hash: snapshot.request_hash.clone(),
                snapshot_id: snapshot.snapshot_id.clone(),
                snapshot_hash: record.snapshot.hash()?,
                request_cursor: request.cursor.clone(),
                page_index,
                previous_page_hash,
                entry,
                next_cursor: next_cursor.clone(),
                complete: scanned.complete,
                issued_at_ms: now,
                expires_at_ms: snapshot.expires_at_ms,
            },
            &signer,
        )?;
        usage.record_signed(&page)?;
        record.position.cursor = next_cursor;
        record.position.previous_page_hash = page.hash()?;
        if scanned.complete {
            let witness = Signed::sign(
                EnrollmentPolicyWitness {
                    version: AUTH_VERSION,
                    purpose: snapshot.purpose,
                    credential_hash: snapshot.credential_hash.clone(),
                    request_hash: snapshot.request_hash.clone(),
                    snapshot_id: snapshot.snapshot_id.clone(),
                    snapshot_hash: record.snapshot.hash()?,
                    target_heads_hash: ObjectHash::body(&snapshot.target_heads)?,
                    final_page_hash: page.hash()?,
                    page_count: record.position.page_count,
                    entry_count: record.position.entry_count,
                    issued_at_ms: now,
                    expires_at_ms: snapshot.expires_at_ms,
                },
                &signer,
            )?;
            usage.record_signed(&witness)?;
            record.witness = Some(witness);
        }
        let result = response(json!({"page":page}), usage, params)?;
        tx.execute("INSERT INTO identity_meta(key,value) VALUES(?,?) ON CONFLICT(key) DO UPDATE SET value=excluded.value",
            params![page_key(&request.snapshot_id), encoded(&CachedPage{page})?])?;
        ensure!(
            tx.execute(
                "UPDATE identity_meta SET value=? WHERE key=? AND value=?",
                params![encoded(&record)?, key, original]
            )? == 1,
            "enrollment policy cursor changed concurrently"
        );
        tx.commit()?;
        Ok(result)
    }

    pub(crate) fn enrollment_policy_witness(
        &mut self,
        identity: &LocalIdentity,
        principal: &str,
        params: &Value,
        now: u64,
    ) -> Result<Value> {
        let _scope = ScopedSignatureVerification::new();
        bounded(params)?;
        let request: EnrollmentPolicyPageRequest = serde_json::from_value(params.clone())?;
        ensure!(
            request.cursor.is_none(),
            "enrollment completion witness takes no cursor"
        );
        let local = PublicKey::parse(&identity.public_key_hex()?)?;
        let noise = PublicKey::parse(principal)?;
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let record: ReadRecord = serde_json::from_str(
            &metadata(&tx, &context_key(&request.snapshot_id))?
                .context("enrollment policy snapshot missing")?,
        )?;
        ensure!(
            record.snapshot.payload.snapshot_id == request.snapshot_id
                && record.request.hash()? == request.request_hash,
            "enrollment policy witness request changed"
        );
        let (_, usage) = verify_record(&tx, &self.policy, &record, &local, &noise, now)?;
        response(
            json!({"witness":record.witness.context("enrollment policy import is incomplete")?}),
            usage,
            params,
        )
    }
}
