//! Bounded public policy imports for the first browser or account Node enrollment.

use super::{
    execution_handoff::ExecutionProofUsage,
    policy_import::{BoundedPolicyImport, VerifiedPolicySnapshot},
    *,
};
use crate::signature_verification::{ScopedSignatureVerification, SignatureVerification};

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum EnrollmentPolicyPurpose {
    BrowserDevice,
    AccountNode,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct EnrollmentPolicyDescriptor {
    pub version: u16,
    pub purpose: EnrollmentPolicyPurpose,
    pub account: AccountId,
    pub requesting_device: DeviceId,
    pub source_node_public_key: PublicKey,
    pub pins: Vec<PolicyPin>,
    pub required_heads: Vec<PolicyStamp>,
    pub target_heads: Vec<PolicyStamp>,
    pub claim_hash: Option<ObjectHash>,
    pub nonce: Nonce,
    pub issued_at_ms: u64,
    pub expires_at_ms: u64,
}

macro_rules! enrollment_policy_object {
    ($name:ident, $domain:literal) => {
        impl sealed::Sealed for $name {}
        impl SignedObject for $name {
            const DOMAIN: &'static str = $domain;
        }
    };
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct EnrollmentPolicyReadRequest {
    pub version: u16,
    pub purpose: EnrollmentPolicyPurpose,
    pub credential_hash: ObjectHash,
    pub descriptor_hash: ObjectHash,
    pub reader_public_key: PublicKey,
    pub target_public_key: PublicKey,
    pub node_public_key: PublicKey,
    pub nonce: Nonce,
    pub issued_at_ms: u64,
    pub expires_at_ms: u64,
}
enrollment_policy_object!(
    EnrollmentPolicyReadRequest,
    "enrollment-policy-read-request-v2"
);

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct EnrollmentPolicySnapshot {
    pub version: u16,
    pub purpose: EnrollmentPolicyPurpose,
    pub credential_hash: ObjectHash,
    pub request_hash: ObjectHash,
    pub snapshot_id: Nonce,
    pub descriptor_hash: ObjectHash,
    pub reader_public_key: PublicKey,
    pub target_public_key: PublicKey,
    pub source_node_public_key: PublicKey,
    pub saved_pins_hash: ObjectHash,
    pub required_heads_hash: ObjectHash,
    pub target_heads: Vec<PolicyStamp>,
    pub issued_at_ms: u64,
    pub expires_at_ms: u64,
}
enrollment_policy_object!(EnrollmentPolicySnapshot, "enrollment-policy-snapshot-v2");

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct EnrollmentPolicyPage {
    pub version: u16,
    pub purpose: EnrollmentPolicyPurpose,
    pub credential_hash: ObjectHash,
    pub request_hash: ObjectHash,
    pub snapshot_id: Nonce,
    pub snapshot_hash: ObjectHash,
    pub page_index: u64,
    pub previous_page_hash: ObjectHash,
    pub entry: Option<JournalEntry>,
    pub next_cursor: Option<ObjectHash>,
    pub complete: bool,
    pub issued_at_ms: u64,
    pub expires_at_ms: u64,
}
enrollment_policy_object!(EnrollmentPolicyPage, "enrollment-policy-page-v2");

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct EnrollmentPolicyWitness {
    pub version: u16,
    pub purpose: EnrollmentPolicyPurpose,
    pub credential_hash: ObjectHash,
    pub request_hash: ObjectHash,
    pub snapshot_id: Nonce,
    pub snapshot_hash: ObjectHash,
    pub target_heads_hash: ObjectHash,
    pub final_page_hash: ObjectHash,
    pub page_count: u64,
    pub entry_count: u64,
    pub issued_at_ms: u64,
    pub expires_at_ms: u64,
}
enrollment_policy_object!(EnrollmentPolicyWitness, "enrollment-policy-witness-v2");

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct EnrollmentPolicyPageRequest {
    pub snapshot_id: Nonce,
    pub request_hash: ObjectHash,
    pub cursor: Option<ObjectHash>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct EnrollmentPolicyCacheBinding {
    pub version: u16,
    pub purpose: EnrollmentPolicyPurpose,
    pub snapshot_id: Nonce,
    pub request_cursor: Option<ObjectHash>,
    pub page_hash: ObjectHash,
}
enrollment_policy_object!(
    EnrollmentPolicyCacheBinding,
    "enrollment-policy-page-cache-v2"
);

fn window(issued: u64, expires: u64, now: u64) -> Result<()> {
    validate_window(issued, expires, now)?;
    require(
        expires - issued <= INVOCATION_MAX_AGE_MS,
        "enrollment policy validity is too long",
    )
}

fn exact_domains(pins: &[PolicyPin], heads: &[PolicyStamp]) -> bool {
    heads.len() == pins.len()
        && heads.windows(2).all(|pair| pair[0].domain < pair[1].domain)
        && heads
            .iter()
            .zip(pins)
            .all(|(head, pin)| head.domain == pin.domain)
}

fn validate_descriptor(descriptor: &EnrollmentPolicyDescriptor, now: u64) -> Result<()> {
    window(descriptor.issued_at_ms, descriptor.expires_at_ms, now)?;
    require(
        descriptor.version == AUTH_VERSION
            && !descriptor.pins.is_empty()
            && descriptor.pins.len() <= MAX_POLICY_DOMAINS
            && descriptor
                .pins
                .windows(2)
                .all(|pair| pair[0].domain < pair[1].domain)
            && descriptor.pins.iter().all(|pin| {
                pin.minimum_head
                    .as_ref()
                    .is_none_or(|head| head.domain == pin.domain)
            })
            && descriptor
                .pins
                .iter()
                .any(|pin| pin.domain == PolicyDomain::Account(descriptor.account.clone()))
            && exact_domains(&descriptor.pins, &descriptor.required_heads)
            && exact_domains(&descriptor.pins, &descriptor.target_heads)
            && descriptor
                .required_heads
                .iter()
                .zip(&descriptor.target_heads)
                .all(|(required, target)| {
                    (required.authority_epoch, required.sequence)
                        <= (target.authority_epoch, target.sequence)
                })
            && match descriptor.purpose {
                EnrollmentPolicyPurpose::BrowserDevice => descriptor.claim_hash.is_none(),
                EnrollmentPolicyPurpose::AccountNode => descriptor.claim_hash.is_some(),
            },
        "invalid enrollment policy descriptor or exact domains",
    )
}

fn finish_usage(usage: &mut ExecutionProofUsage) -> Result<()> {
    if let Some(counts) = SignatureVerification::current_counts() {
        if counts.actual_verifications > MAX_PROOF_SIGNATURES {
            return Err(AuthorizationError::BudgetExceeded);
        }
        usage.record_actual_signature_counts(counts);
    }
    usage.enforce_limits()
}

/// Checks the public carrier; the purpose wrapper must still verify current enrollment rights.
pub fn verify_enrollment_policy_read_request(
    request: &Signed<EnrollmentPolicyReadRequest>,
    descriptor: &EnrollmentPolicyDescriptor,
    credential_hash: &ObjectHash,
    local_node: &PublicKey,
    noise: &PublicKey,
    now: u64,
) -> Result<ExecutionProofUsage> {
    let _scope = ScopedSignatureVerification::new();
    require(
        canonical(&(request, descriptor))?.len() <= MAX_PROOF_BYTES,
        "enrollment policy read carrier exceeds 64 KiB",
    )?;
    validate_descriptor(descriptor, now)?;
    let value = &request.payload;
    window(value.issued_at_ms, value.expires_at_ms, now)?;
    require(
        value.version == AUTH_VERSION
            && value.purpose == descriptor.purpose
            && value.credential_hash == *credential_hash
            && value.descriptor_hash == ObjectHash::body(descriptor)?
            && request.issuer == *noise
            && value.reader_public_key == *noise
            && value.node_public_key == *local_node
            && descriptor.source_node_public_key == *local_node
            && value.issued_at_ms >= descriptor.issued_at_ms
            && value.expires_at_ms <= descriptor.expires_at_ms
            && (descriptor.purpose != EnrollmentPolicyPurpose::AccountNode
                || DeviceId::from_key(noise) == descriptor.requesting_device),
        "enrollment policy request changed its credential, descriptor or Noise reader",
    )?;
    request.verify_signature()?;
    let mut usage = ExecutionProofUsage::default();
    usage.record_metadata(descriptor)?;
    usage.record_signed(request)?;
    finish_usage(&mut usage)?;
    Ok(usage)
}

#[derive(Clone, Debug)]
pub(crate) struct EnrollmentPolicyImport {
    descriptor: EnrollmentPolicyDescriptor,
    request: Signed<EnrollmentPolicyReadRequest>,
    snapshot: Signed<EnrollmentPolicySnapshot>,
    request_hash: ObjectHash,
    snapshot_hash: ObjectHash,
    import: BoundedPolicyImport,
    page_count: u64,
    entry_count: u64,
    previous_page_hash: ObjectHash,
    next_cursor: Option<ObjectHash>,
    last_page: Option<Signed<EnrollmentPolicyPage>>,
    complete: bool,
    usage: ExecutionProofUsage,
}

impl EnrollmentPolicyImport {
    pub(crate) fn begin(
        descriptor: EnrollmentPolicyDescriptor,
        credential_hash: &ObjectHash,
        request: Signed<EnrollmentPolicyReadRequest>,
        snapshot: Signed<EnrollmentPolicySnapshot>,
        reader: &PublicKey,
        target: &PublicKey,
        source: &PublicKey,
        now: u64,
    ) -> Result<Self> {
        let _scope = ScopedSignatureVerification::new();
        require(
            canonical(&(&descriptor, &request, &snapshot))?.len() <= MAX_PROOF_BYTES,
            "enrollment policy begin carrier exceeds 64 KiB",
        )?;
        let mut usage = verify_enrollment_policy_read_request(
            &request,
            &descriptor,
            credential_hash,
            source,
            reader,
            now,
        )?;
        let request_hash = request.hash()?;
        let header = &snapshot.payload;
        window(header.issued_at_ms, header.expires_at_ms, now)?;
        require(
            request.payload.reader_public_key == *reader
                && request.payload.target_public_key == *target
                && snapshot.issuer == *source
                && header.version == AUTH_VERSION
                && header.purpose == descriptor.purpose
                && header.credential_hash == *credential_hash
                && header.request_hash == request_hash
                && header.descriptor_hash == request.payload.descriptor_hash
                && header.reader_public_key == *reader
                && header.target_public_key == *target
                && header.source_node_public_key == *source
                && header.saved_pins_hash == ObjectHash::body(&descriptor.pins)?
                && header.required_heads_hash == ObjectHash::body(&descriptor.required_heads)?
                && header.target_heads == descriptor.target_heads
                && header.issued_at_ms >= request.payload.issued_at_ms
                && header.expires_at_ms <= request.payload.expires_at_ms,
            "enrollment policy snapshot changed its exact request or independently fixed trust",
        )?;
        snapshot.verify_signature()?;
        let import =
            BoundedPolicyImport::new(descriptor.pins.clone(), descriptor.required_heads.clone())?;
        let snapshot_hash = snapshot.hash()?;
        usage.record_signed(&snapshot)?;
        finish_usage(&mut usage)?;
        Ok(Self {
            descriptor,
            request,
            snapshot,
            request_hash,
            previous_page_hash: snapshot_hash.clone(),
            snapshot_hash,
            import,
            page_count: 0,
            entry_count: 0,
            next_cursor: None,
            last_page: None,
            complete: false,
            usage,
        })
    }

    pub(crate) fn append_page(
        &mut self,
        page: Signed<EnrollmentPolicyPage>,
        now: u64,
    ) -> Result<()> {
        let _scope = ScopedSignatureVerification::new();
        require(
            canonical(&page)?.len() <= MAX_PROOF_BYTES,
            "enrollment policy page carrier exceeds 64 KiB",
        )?;
        window(
            self.snapshot.payload.issued_at_ms,
            self.snapshot.payload.expires_at_ms,
            now,
        )?;
        let value = &page.payload;
        window(value.issued_at_ms, value.expires_at_ms, now)?;
        require(
            page.issuer == self.snapshot.issuer
                && value.version == AUTH_VERSION
                && value.purpose == self.descriptor.purpose
                && value.credential_hash == self.request.payload.credential_hash
                && value.request_hash == self.request_hash
                && value.snapshot_id == self.snapshot.payload.snapshot_id
                && value.snapshot_hash == self.snapshot_hash
                && value.issued_at_ms >= self.snapshot.payload.issued_at_ms
                && value.expires_at_ms <= self.snapshot.payload.expires_at_ms,
            "enrollment policy page changed its purpose, source or snapshot",
        )?;
        page.verify_signature()?;
        let mut usage = ExecutionProofUsage::default();
        usage.record_signed(&page)?;
        if let Some(entry) = &value.entry {
            usage.merge(&policy_proof::policy_entry_payload_usage(entry)?);
        }
        usage.enforce_limits()?;
        if self.last_page.as_ref() == Some(&page) {
            finish_usage(&mut usage)?;
            self.usage = usage;
            return Ok(());
        }
        require(
            !self.complete
                && value.page_index == self.page_count
                && value.previous_page_hash == self.previous_page_hash
                && value.complete == value.next_cursor.is_none(),
            "enrollment policy page is missing, duplicated or out of order",
        )?;
        let mut staged = self.import.clone();
        if let Some(entry) = &value.entry {
            staged.apply_entry(entry.clone())?;
        }
        let next_page_count = self
            .page_count
            .checked_add(1)
            .ok_or(AuthorizationError::BudgetExceeded)?;
        let next_entry_count = self
            .entry_count
            .checked_add(u64::from(value.entry.is_some()))
            .ok_or(AuthorizationError::BudgetExceeded)?;
        let hash = page.hash()?;
        finish_usage(&mut usage)?;
        self.import = staged;
        self.page_count = next_page_count;
        self.entry_count = next_entry_count;
        self.previous_page_hash = hash;
        self.next_cursor = value.next_cursor.clone();
        self.complete = value.complete;
        self.last_page = Some(page);
        self.usage = usage;
        Ok(())
    }

    pub(crate) fn finish(
        &mut self,
        witness: &Signed<EnrollmentPolicyWitness>,
        now: u64,
    ) -> Result<VerifiedPolicySnapshot> {
        let _scope = ScopedSignatureVerification::new();
        require(
            canonical(witness)?.len() <= MAX_PROOF_BYTES,
            "enrollment policy witness carrier exceeds 64 KiB",
        )?;
        window(
            self.snapshot.payload.issued_at_ms,
            self.snapshot.payload.expires_at_ms,
            now,
        )?;
        let value = &witness.payload;
        window(value.issued_at_ms, value.expires_at_ms, now)?;
        require(
            self.complete
                && witness.issuer == self.snapshot.issuer
                && value.version == AUTH_VERSION
                && value.purpose == self.descriptor.purpose
                && value.credential_hash == self.request.payload.credential_hash
                && value.request_hash == self.request_hash
                && value.snapshot_id == self.snapshot.payload.snapshot_id
                && value.snapshot_hash == self.snapshot_hash
                && value.target_heads_hash == ObjectHash::body(&self.descriptor.target_heads)?
                && value.final_page_hash == self.previous_page_hash
                && value.page_count == self.page_count
                && value.entry_count == self.entry_count
                && value.issued_at_ms >= self.snapshot.payload.issued_at_ms
                && value.expires_at_ms <= self.snapshot.payload.expires_at_ms,
            "enrollment policy witness changed its purpose or completed snapshot",
        )?;
        witness.verify_signature()?;
        let mut usage = ExecutionProofUsage::default();
        usage.record_signed(witness)?;
        let snapshot = self.import.clone().finish(&self.descriptor.target_heads)?;
        finish_usage(&mut usage)?;
        self.usage = usage;
        Ok(snapshot)
    }

    pub(crate) fn page_request(&self) -> EnrollmentPolicyPageRequest {
        EnrollmentPolicyPageRequest {
            snapshot_id: self.snapshot.payload.snapshot_id.clone(),
            request_hash: self.request_hash.clone(),
            cursor: self.next_cursor.clone(),
        }
    }

    pub(crate) fn last_proof_usage(&self) -> &ExecutionProofUsage {
        &self.usage
    }

    pub(crate) fn descriptor(&self) -> &EnrollmentPolicyDescriptor {
        &self.descriptor
    }

    pub(crate) fn request(&self) -> &Signed<EnrollmentPolicyReadRequest> {
        &self.request
    }

    pub(crate) fn snapshot(&self) -> &Signed<EnrollmentPolicySnapshot> {
        &self.snapshot
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::authorization::tests::{Fixture, NOW};

    fn key(byte: u8) -> Identity {
        Identity::from_secret_hex(&hex::encode([byte; 32])).unwrap()
    }

    struct Flow {
        descriptor: EnrollmentPolicyDescriptor,
        credential_hash: ObjectHash,
        request: Signed<EnrollmentPolicyReadRequest>,
        snapshot: Signed<EnrollmentPolicySnapshot>,
        pages: Vec<Signed<EnrollmentPolicyPage>>,
        witness: Signed<EnrollmentPolicyWitness>,
        reader: Identity,
        source: Identity,
    }

    impl Flow {
        fn new(purpose: EnrollmentPolicyPurpose) -> Self {
            let fixture = Fixture::new();
            let reader = key(71);
            let target = if purpose == EnrollmentPolicyPurpose::BrowserDevice {
                PublicKey::of(&reader)
            } else {
                PublicKey::of(&key(72))
            };
            let source = fixture.node.clone();
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
            let target_heads = vec![fixture.state.head(&domain).unwrap().clone()];
            let descriptor = EnrollmentPolicyDescriptor {
                version: AUTH_VERSION,
                purpose,
                account: fixture.account.clone(),
                requesting_device: DeviceId::from_key(&PublicKey::of(&reader)),
                source_node_public_key: PublicKey::of(&source),
                pins: vec![PolicyPin {
                    domain: domain.clone(),
                    genesis_root: PublicKey::of(&fixture.root),
                    minimum_head: Some(target_heads[0].clone()),
                }],
                required_heads: target_heads.clone(),
                target_heads,
                claim_hash: (purpose == EnrollmentPolicyPurpose::AccountNode)
                    .then(|| ObjectHash::body(&"original claim").unwrap()),
                nonce: Nonce::from_bytes([73; 32]),
                issued_at_ms: NOW,
                expires_at_ms: NOW + 1000,
            };
            let credential_hash = ObjectHash::body(&"independently fixed credential").unwrap();
            let request = Signed::sign(
                EnrollmentPolicyReadRequest {
                    version: AUTH_VERSION,
                    purpose,
                    credential_hash: credential_hash.clone(),
                    descriptor_hash: ObjectHash::body(&descriptor).unwrap(),
                    reader_public_key: PublicKey::of(&reader),
                    target_public_key: target.clone(),
                    node_public_key: PublicKey::of(&source),
                    nonce: Nonce::from_bytes([74; 32]),
                    issued_at_ms: NOW,
                    expires_at_ms: NOW + 900,
                },
                &reader,
            )
            .unwrap();
            let snapshot = Signed::sign(
                EnrollmentPolicySnapshot {
                    version: AUTH_VERSION,
                    purpose,
                    credential_hash: credential_hash.clone(),
                    request_hash: request.hash().unwrap(),
                    snapshot_id: Nonce::from_bytes([75; 32]),
                    descriptor_hash: ObjectHash::body(&descriptor).unwrap(),
                    reader_public_key: PublicKey::of(&reader),
                    target_public_key: target,
                    source_node_public_key: PublicKey::of(&source),
                    saved_pins_hash: ObjectHash::body(&descriptor.pins).unwrap(),
                    required_heads_hash: ObjectHash::body(&descriptor.required_heads).unwrap(),
                    target_heads: descriptor.target_heads.clone(),
                    issued_at_ms: NOW,
                    expires_at_ms: NOW + 800,
                },
                &source,
            )
            .unwrap();
            let mut pages = Vec::new();
            let mut previous = snapshot.hash().unwrap();
            for (index, entry) in entries.iter().enumerate() {
                let complete = index + 1 == entries.len();
                let page = Signed::sign(
                    EnrollmentPolicyPage {
                        version: AUTH_VERSION,
                        purpose,
                        credential_hash: credential_hash.clone(),
                        request_hash: request.hash().unwrap(),
                        snapshot_id: snapshot.payload.snapshot_id.clone(),
                        snapshot_hash: snapshot.hash().unwrap(),
                        page_index: index as u64,
                        previous_page_hash: previous,
                        entry: Some(entry.clone()),
                        next_cursor: (!complete).then(|| ObjectHash::body(&index).unwrap()),
                        complete,
                        issued_at_ms: NOW,
                        expires_at_ms: NOW + 800,
                    },
                    &source,
                )
                .unwrap();
                previous = page.hash().unwrap();
                pages.push(page);
            }
            let witness = Signed::sign(
                EnrollmentPolicyWitness {
                    version: AUTH_VERSION,
                    purpose,
                    credential_hash: credential_hash.clone(),
                    request_hash: request.hash().unwrap(),
                    snapshot_id: snapshot.payload.snapshot_id.clone(),
                    snapshot_hash: snapshot.hash().unwrap(),
                    target_heads_hash: ObjectHash::body(&descriptor.target_heads).unwrap(),
                    final_page_hash: previous,
                    page_count: pages.len() as u64,
                    entry_count: entries.len() as u64,
                    issued_at_ms: NOW,
                    expires_at_ms: NOW + 800,
                },
                &source,
            )
            .unwrap();
            Self {
                descriptor,
                credential_hash,
                request,
                snapshot,
                pages,
                witness,
                reader,
                source,
            }
        }

        fn begin(&self) -> Result<EnrollmentPolicyImport> {
            EnrollmentPolicyImport::begin(
                self.descriptor.clone(),
                &self.credential_hash,
                self.request.clone(),
                self.snapshot.clone(),
                &PublicKey::of(&self.reader),
                &self.request.payload.target_public_key,
                &PublicKey::of(&self.source),
                NOW,
            )
        }
    }

    #[test]
    fn both_purposes_import_real_history_with_fresh_per_carrier_crypto() {
        for purpose in [
            EnrollmentPolicyPurpose::BrowserDevice,
            EnrollmentPolicyPurpose::AccountNode,
        ] {
            let flow = Flow::new(purpose);
            let mut import = flow.begin().unwrap();
            assert_eq!(
                import
                    .last_proof_usage()
                    .counts()
                    .actual
                    .unwrap()
                    .actual_verifications,
                2
            );
            for page in &flow.pages {
                import.append_page(page.clone(), NOW).unwrap();
                let expected = import.page_request();
                import.append_page(page.clone(), NOW).unwrap();
                assert_eq!(import.page_request(), expected);
                assert_eq!(
                    import
                        .last_proof_usage()
                        .counts()
                        .actual
                        .unwrap()
                        .actual_verifications,
                    1
                );
            }
            let state = import.finish(&flow.witness, NOW).unwrap();
            assert_eq!(state.heads(), flow.descriptor.target_heads);
            assert_eq!(state.journal().len(), flow.pages.len());
            assert_eq!(
                import
                    .last_proof_usage()
                    .counts()
                    .actual
                    .unwrap()
                    .actual_verifications,
                1
            );
        }
    }

    #[test]
    fn changed_purpose_or_expected_key_cannot_begin() {
        let flow = Flow::new(EnrollmentPolicyPurpose::BrowserDevice);
        let mut request = flow.request.payload.clone();
        request.purpose = EnrollmentPolicyPurpose::AccountNode;
        let request = Signed::sign(request, &flow.reader).unwrap();
        assert!(
            verify_enrollment_policy_read_request(
                &request,
                &flow.descriptor,
                &flow.credential_hash,
                &PublicKey::of(&flow.source),
                &PublicKey::of(&flow.reader),
                NOW,
            )
            .is_err()
        );
        assert!(
            EnrollmentPolicyImport::begin(
                flow.descriptor.clone(),
                &flow.credential_hash,
                flow.request.clone(),
                flow.snapshot.clone(),
                &PublicKey::of(&flow.reader),
                &PublicKey::of(&key(79)),
                &PublicKey::of(&flow.source),
                NOW,
            )
            .is_err()
        );
        let mut snapshot = flow.snapshot.payload.clone();
        snapshot.purpose = EnrollmentPolicyPurpose::AccountNode;
        let snapshot = Signed::sign(snapshot, &flow.source).unwrap();
        assert!(
            EnrollmentPolicyImport::begin(
                flow.descriptor.clone(),
                &flow.credential_hash,
                flow.request.clone(),
                snapshot,
                &PublicKey::of(&flow.reader),
                &flow.request.payload.target_public_key,
                &PublicKey::of(&flow.source),
                NOW,
            )
            .is_err()
        );
    }

    #[test]
    fn mixed_reordered_or_invalid_pages_and_witnesses_do_not_advance() {
        let flow = Flow::new(EnrollmentPolicyPurpose::BrowserDevice);
        let mut import = flow.begin().unwrap();
        let before = import.page_request();
        let mut mixed = flow.pages[0].payload.clone();
        mixed.snapshot_hash = ObjectHash::body(&"another snapshot").unwrap();
        assert!(
            import
                .append_page(Signed::sign(mixed, &flow.source).unwrap(), NOW)
                .is_err()
        );
        assert_eq!(import.page_request(), before);
        assert!(import.append_page(flow.pages[1].clone(), NOW).is_err());
        assert_eq!(import.page_request(), before);
        let mut invalid = flow.pages[0].payload.clone();
        let Some(JournalEntry::AccountGenesis(value)) = &mut invalid.entry else {
            unreachable!()
        };
        value.signature = Signature::parse(&"00".repeat(64)).unwrap();
        assert!(
            import
                .append_page(Signed::sign(invalid, &flow.source).unwrap(), NOW)
                .is_err()
        );
        assert_eq!(import.page_request(), before);
        for page in &flow.pages {
            import.append_page(page.clone(), NOW).unwrap();
        }
        let completed = import.page_request();
        let mut witness = flow.witness.payload.clone();
        witness.purpose = EnrollmentPolicyPurpose::AccountNode;
        assert!(
            import
                .finish(&Signed::sign(witness, &flow.source).unwrap(), NOW)
                .is_err()
        );
        assert_eq!(import.page_request(), completed);
        assert_eq!(
            import.finish(&flow.witness, NOW).unwrap().heads(),
            flow.descriptor.target_heads
        );
    }

    #[test]
    fn cache_binding_signature_covers_the_exact_snapshot_cursor_and_page() {
        let source = key(71);
        let binding = Signed::sign(
            EnrollmentPolicyCacheBinding {
                version: AUTH_VERSION,
                purpose: EnrollmentPolicyPurpose::BrowserDevice,
                snapshot_id: Nonce::from_bytes([72; 32]),
                request_cursor: Some(ObjectHash::body(&"original cursor").unwrap()),
                page_hash: ObjectHash::body(&"original signed page").unwrap(),
            },
            &source,
        )
        .unwrap();
        binding.verify_signature().unwrap();
        for field in 0..5 {
            let mut changed = binding.clone();
            match field {
                0 => changed.payload.version += 1,
                1 => changed.payload.purpose = EnrollmentPolicyPurpose::AccountNode,
                2 => changed.payload.snapshot_id = Nonce::from_bytes([73; 32]),
                3 => changed.payload.request_cursor = None,
                4 => changed.payload.page_hash = ObjectHash::body(&"another page").unwrap(),
                _ => unreachable!(),
            }
            assert!(changed.verify_signature().is_err(), "field {field}");
        }
    }
}
