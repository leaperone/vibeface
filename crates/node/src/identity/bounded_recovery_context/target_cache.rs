use anyhow::{Context, Result, ensure};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use vibeface_secure::authorization::{
    AUTH_VERSION, AuthorityBinding, Nonce, ObjectHash, PolicyDomain, Signature, Signed,
    bootstrap::recovery::{cross_node::CrossNodeRecoveryRequest, stream::*},
};

const MAX_CACHE_ROW_BYTES: usize = 65_536;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct UntrustedTargetRecoveryHeader {
    pub(crate) request: RecoveryReadRequest,
    pub(crate) snapshot: Signed<RecoveryContextSnapshot>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct UntrustedTargetRecoveryApproval {
    pub(crate) possession: StreamedCrossNodeRecoveryPossessionRequest,
    pub(crate) proof: Signature,
    pub(crate) approval: Signed<CrossNodeRecoveryImportApproval>,
}

/// Stored carriers do not authorize recovery. The native importer must consume each page again.
pub(crate) struct UntrustedTargetRecoveryCache {
    request: Signed<CrossNodeRecoveryRequest>,
    snapshot_id: Nonce,
    scope_key: String,
}

impl UntrustedTargetRecoveryCache {
    pub(crate) fn new(
        request: &Signed<CrossNodeRecoveryRequest>,
        snapshot_id: &Nonce,
    ) -> Result<Self> {
        request.verify_signature()?;
        request.payload.target_claim.verify_signature()?;
        let original = &request.payload;
        let claim = &original.target_claim;
        ensure!(
            original.version == AUTH_VERSION
                && claim.payload.version == AUTH_VERSION
                && claim.issuer == claim.payload.node_public_key
                && original.source_node_public_key != claim.payload.node_public_key,
            "target recovery request or claim binding changed"
        );
        let scope = ObjectHash::body(&(
            "target-cross-recovery-cache-v2",
            &original.account,
            &original.space,
            &original.new_device_public_key,
            &claim.payload.node_public_key,
            &original.source_node_public_key,
            snapshot_id,
            request.hash()?,
        ))?;
        Ok(Self {
            request: request.clone(),
            snapshot_id: snapshot_id.clone(),
            scope_key: format!("target-cross-recovery:{scope}"),
        })
    }

    pub(crate) fn store_header(
        &self,
        connection: &Connection,
        snapshot: &Signed<RecoveryContextSnapshot>,
    ) -> Result<()> {
        let header = UntrustedTargetRecoveryHeader {
            request: RecoveryReadRequest::CrossNode(self.request.clone()),
            snapshot: snapshot.clone(),
        };
        self.check_header(&header)?;
        write_immutable(connection, &self.header_key(), &header)
    }

    pub(crate) fn load_header(
        &self,
        connection: &Connection,
    ) -> Result<Option<UntrustedTargetRecoveryHeader>> {
        let header = read_json(connection, &self.header_key())?;
        if let Some(header) = &header {
            self.check_header(header)?;
        }
        Ok(header)
    }

    pub(crate) fn store_page(
        &self,
        connection: &Connection,
        page: &Signed<RecoveryContextPage>,
    ) -> Result<()> {
        let header = self.required_header(connection)?;
        let index = page.payload.page_index;
        self.check_page(&header, page, index)?;
        if let Some(previous_index) = index.checked_sub(1) {
            let previous = self
                .read_page(connection, &header, previous_index)?
                .context("target recovery previous page is missing")?;
            ensure!(
                !previous.payload.complete && page.payload.previous_page_hash == previous.hash()?,
                "target recovery page skipped or changed its previous page"
            );
        }
        write_immutable(connection, &self.page_key(index), page)
    }

    pub(crate) fn load_page(
        &self,
        connection: &Connection,
        index: u64,
    ) -> Result<Option<Signed<RecoveryContextPage>>> {
        let header = self.required_header(connection)?;
        self.read_page(connection, &header, index)
    }

    pub(crate) fn store_witness(
        &self,
        connection: &Connection,
        witness: &Signed<RecoveryContextWitness>,
    ) -> Result<()> {
        let header = self.required_header(connection)?;
        self.check_witness(connection, &header, witness)?;
        write_immutable(connection, &self.witness_key(), witness)
    }

    pub(crate) fn load_witness(
        &self,
        connection: &Connection,
    ) -> Result<Option<Signed<RecoveryContextWitness>>> {
        let header = self.required_header(connection)?;
        let witness = read_json(connection, &self.witness_key())?;
        if let Some(witness) = &witness {
            self.check_witness(connection, &header, witness)?;
        }
        Ok(witness)
    }

    pub(crate) fn store_approval(
        &self,
        connection: &Connection,
        approval: &UntrustedTargetRecoveryApproval,
    ) -> Result<()> {
        self.check_approval(connection, approval)?;
        write_immutable(connection, &self.approval_key(), approval)
    }

    pub(crate) fn load_approval(
        &self,
        connection: &Connection,
    ) -> Result<Option<UntrustedTargetRecoveryApproval>> {
        let approval = read_json(connection, &self.approval_key())?;
        if let Some(approval) = &approval {
            self.check_approval(connection, approval)?;
        }
        Ok(approval)
    }

    pub(crate) fn store_committed(
        &self,
        connection: &Connection,
        committed: &StreamedCrossNodeRecoveryCommitted,
    ) -> Result<()> {
        self.check_committed(connection, committed)?;
        write_immutable(connection, &self.committed_key(), committed)
    }

    pub(crate) fn load_committed(
        &self,
        connection: &Connection,
    ) -> Result<Option<StreamedCrossNodeRecoveryCommitted>> {
        let committed = read_json(connection, &self.committed_key())?;
        if let Some(committed) = &committed {
            self.check_committed(connection, committed)?;
        }
        Ok(committed)
    }

    fn header_key(&self) -> String {
        format!("{}:header", self.scope_key)
    }

    fn page_key(&self, index: u64) -> String {
        format!("{}:page:{index}", self.scope_key)
    }

    fn witness_key(&self) -> String {
        format!("{}:witness", self.scope_key)
    }

    fn approval_key(&self) -> String {
        format!("{}:approval", self.scope_key)
    }

    fn committed_key(&self) -> String {
        format!("{}:committed", self.scope_key)
    }

    fn required_header(&self, connection: &Connection) -> Result<UntrustedTargetRecoveryHeader> {
        self.load_header(connection)?
            .context("target recovery header is missing")
    }

    fn check_header(&self, header: &UntrustedTargetRecoveryHeader) -> Result<()> {
        let snapshot = &header.snapshot.payload;
        let original = &self.request.payload;
        ensure!(
            header.request == RecoveryReadRequest::CrossNode(self.request.clone())
                && header.snapshot.issuer == original.source_node_public_key
                && snapshot.version == AUTH_VERSION
                && snapshot.kind == RecoveryContextKind::CrossNode
                && snapshot.request_hash == self.request.hash()?
                && snapshot.snapshot_id == self.snapshot_id
                && snapshot.saved_pins_hash == header.request.saved_pins_hash()?
                && snapshot.source_node_public_key == original.source_node_public_key
                && snapshot.claim == original.target_claim
                && original.issued_at_ms <= snapshot.issued_at_ms
                && snapshot.issued_at_ms < snapshot.expires_at_ms
                && snapshot.expires_at_ms == original.expires_at_ms,
            "target recovery header changed its original request or snapshot"
        );
        header.snapshot.verify_signature()?;
        Ok(())
    }

    fn check_page(
        &self,
        header: &UntrustedTargetRecoveryHeader,
        page: &Signed<RecoveryContextPage>,
        index: u64,
    ) -> Result<()> {
        let snapshot = &header.snapshot.payload;
        let body = &page.payload;
        ensure!(
            page.issuer == snapshot.source_node_public_key
                && body.version == AUTH_VERSION
                && body.kind == RecoveryContextKind::CrossNode
                && body.request_hash == snapshot.request_hash
                && body.snapshot_id == snapshot.snapshot_id
                && body.target_heads_hash == recovery_target_heads_hash(&snapshot.target_heads)?
                && body.page_index == index
                && body.complete == body.next_cursor.is_none()
                && (index != 0 || body.previous_page_hash == header.snapshot.hash()?),
            "target recovery page changed its snapshot, index or source"
        );
        page.verify_signature()?;
        Ok(())
    }

    fn read_page(
        &self,
        connection: &Connection,
        header: &UntrustedTargetRecoveryHeader,
        index: u64,
    ) -> Result<Option<Signed<RecoveryContextPage>>> {
        let page = read_json(connection, &self.page_key(index))?;
        if let Some(page) = &page {
            self.check_page(header, page, index)?;
        }
        Ok(page)
    }

    fn check_witness(
        &self,
        connection: &Connection,
        header: &UntrustedTargetRecoveryHeader,
        witness: &Signed<RecoveryContextWitness>,
    ) -> Result<()> {
        let snapshot = &header.snapshot.payload;
        let body = &witness.payload;
        ensure!(
            witness.issuer == snapshot.source_node_public_key
                && body.version == AUTH_VERSION
                && body.kind == RecoveryContextKind::CrossNode
                && body.request_hash == snapshot.request_hash
                && body.snapshot_id == snapshot.snapshot_id
                && body.target_heads == snapshot.target_heads
                && body.claim_hash == snapshot.claim.hash()?
                && body.source_node_public_key == snapshot.source_node_public_key
                && snapshot.issued_at_ms <= body.issued_at_ms
                && body.issued_at_ms < body.expires_at_ms
                && body.expires_at_ms == snapshot.expires_at_ms,
            "target recovery witness changed its snapshot or source"
        );
        witness.verify_signature()?;
        let index = body
            .page_count
            .checked_sub(1)
            .context("target recovery witness has no final page")?;
        let final_page = self
            .read_page(connection, header, index)?
            .context("target recovery final page is missing")?;
        ensure!(
            final_page.payload.complete && final_page.hash()? == body.final_page_hash,
            "target recovery witness changed its final page"
        );
        Ok(())
    }

    fn check_reference(
        &self,
        connection: &Connection,
        header: &UntrustedTargetRecoveryHeader,
        reference: &RecoveryContextReference,
    ) -> Result<()> {
        let witness: Signed<RecoveryContextWitness> =
            read_json(connection, &self.witness_key())?
                .context("target recovery witness is missing")?;
        self.check_witness(connection, header, &witness)?;
        ensure!(
            reference.snapshot_id == self.snapshot_id
                && reference.request_hash == self.request.hash()?
                && reference.snapshot_hash == header.snapshot.hash()?
                && reference.witness_hash == witness.hash()?,
            "target recovery carrier changed its completed context"
        );
        Ok(())
    }

    fn check_approval(
        &self,
        connection: &Connection,
        carrier: &UntrustedTargetRecoveryApproval,
    ) -> Result<()> {
        let header = self.required_header(connection)?;
        let possession = &carrier.possession;
        self.check_reference(connection, &header, &possession.context)?;
        let original = &self.request.payload;
        let node = &possession.node_request;
        let target = &original.target_claim.payload.node_public_key;
        let approval = &carrier.approval.payload;
        ensure!(
            possession.request == self.request
                && node.claim == original.target_claim
                && node.expected_account_head.domain
                    == PolicyDomain::Account(original.account.clone())
                && node.expected_space_head.domain == PolicyDomain::Space(original.space.clone())
                && header.snapshot.payload.target_heads
                    == [
                        node.expected_account_head.clone(),
                        node.expected_space_head.clone()
                    ]
                && node.account_recovery.payload.account == original.account
                && node.account_recovery.payload.current_root == original.new_root_public_key
                && node.account_recovery.issuer == self.request.issuer
                && node.account_recovery.payload.authority
                    == AuthorityBinding::from_key(target.clone())
                && node.node_certificate.public_key == *target
                && possession.source_certificate.public_key == original.source_node_public_key
                && carrier.approval.issuer == *target
                && approval.version == AUTH_VERSION
                && approval.context == possession.context
                && approval.account == original.account
                && approval.space == original.space
                && approval.new_device_public_key == original.new_device_public_key
                && approval.new_root_public_key == original.new_root_public_key
                && approval.source_node_public_key == original.source_node_public_key
                && approval.target_node_public_key == *target
                && approval.possession_hash == ObjectHash::body(possession)?
                && approval.claim_hash == original.target_claim.hash()?
                && approval.expires_at_ms == original.expires_at_ms,
            "target recovery approval changed its original possession or scope"
        );
        node.account_recovery.verify_signature()?;
        carrier.approval.verify_signature()?;
        Ok(())
    }

    fn check_committed(
        &self,
        connection: &Connection,
        committed: &StreamedCrossNodeRecoveryCommitted,
    ) -> Result<()> {
        let approval = self
            .load_approval(connection)?
            .context("target recovery approval is missing")?;
        let original = &self.request.payload;
        let receipt = &committed.receipt.payload;
        let node = &committed.request.possession.node_request;
        ensure!(
            committed.request.possession == approval.possession
                && committed.receipt.issuer == original.source_node_public_key
                && receipt.version == AUTH_VERSION
                && receipt.context == approval.possession.context
                && receipt.request_hash == self.request.hash()?
                && receipt.input_hash == ObjectHash::body(&committed.request)?
                && receipt.expected_account_head == node.expected_account_head
                && receipt.expected_space_head == node.expected_space_head
                && receipt.committed_account_head.domain
                    == PolicyDomain::Account(original.account.clone())
                && receipt.committed_space_head.domain
                    == PolicyDomain::Space(original.space.clone())
                && receipt.target_node_public_key == original.target_claim.payload.node_public_key
                && receipt.new_device_public_key == original.new_device_public_key
                && receipt.grant_leaf == committed.request.grant_leaf,
            "target recovery completion changed its original approval or input"
        );
        committed.receipt.verify_signature()?;
        Ok(())
    }
}

fn read_json<T: DeserializeOwned>(connection: &Connection, key: &str) -> Result<Option<T>> {
    let length: Option<usize> = connection
        .query_row(
            "SELECT length(CAST(value AS BLOB)) FROM identity_meta WHERE key=?",
            [key],
            |row| row.get(0),
        )
        .optional()?;
    let Some(length) = length else {
        return Ok(None);
    };
    ensure!(
        length <= MAX_CACHE_ROW_BYTES,
        "stored target recovery carrier exceeds 64 KiB"
    );
    let encoded: String = connection
        .query_row(
            "SELECT value FROM identity_meta WHERE key=? AND length(CAST(value AS BLOB))<=?",
            params![key, MAX_CACHE_ROW_BYTES],
            |row| row.get(0),
        )
        .optional()?
        .context("target recovery carrier changed while reading")?;
    Ok(Some(serde_json::from_str(&encoded)?))
}

fn write_immutable<T>(connection: &Connection, key: &str, value: &T) -> Result<()>
where
    T: Serialize + DeserializeOwned + PartialEq,
{
    let encoded = serde_json::to_string(value)?;
    ensure!(
        encoded.len() <= MAX_CACHE_ROW_BYTES,
        "target recovery carrier exceeds 64 KiB"
    );
    connection.execute(
        "INSERT INTO identity_meta(key,value) VALUES(?,?) ON CONFLICT(key) DO NOTHING",
        params![key, encoded],
    )?;
    ensure!(
        read_json::<T>(connection, key)?.as_ref() == Some(value),
        "target recovery carrier conflicts with its stored original"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use vibeface_secure::{
        Identity,
        authorization::{
            AccountDocument, AccountId, Actor, DeviceCertificate, DeviceId, DeviceUse, PolicyPin,
            PolicyStamp, PublicKey, RecoveryPolicy, ServiceId, SpaceId,
            bootstrap::{NodeClaim, recovery::RecoveryNodeRequest},
        },
    };

    struct Fixture {
        source: Identity,
        target: Identity,
        recovery: Identity,
        request: Signed<CrossNodeRecoveryRequest>,
        snapshot: Signed<RecoveryContextSnapshot>,
    }

    impl Fixture {
        fn new() -> Result<Self> {
            let root = Identity::generate().map_err(anyhow::Error::msg)?;
            let source = Identity::generate().map_err(anyhow::Error::msg)?;
            let target = Identity::generate().map_err(anyhow::Error::msg)?;
            let client = Identity::generate().map_err(anyhow::Error::msg)?;
            let recovery = Identity::generate().map_err(anyhow::Error::msg)?;
            let genesis_root = PublicKey::of(&root);
            let account = AccountId::from_root(&genesis_root);
            let space = SpaceId::personal(&account);
            let target_heads: Vec<_> = [
                PolicyDomain::Account(account.clone()),
                PolicyDomain::Space(space.clone()),
            ]
            .into_iter()
            .map(|domain| -> Result<PolicyStamp> {
                Ok(PolicyStamp {
                    head: ObjectHash::body(&domain)?,
                    domain,
                    authority_epoch: 0,
                    sequence: 0,
                })
            })
            .collect::<Result<_>>()?;
            let saved_pins = target_heads
                .iter()
                .map(|head| PolicyPin {
                    domain: head.domain.clone(),
                    genesis_root: genesis_root.clone(),
                    minimum_head: Some(head.clone()),
                })
                .collect();
            let claim = Signed::sign(
                NodeClaim {
                    version: AUTH_VERSION,
                    node_public_key: PublicKey::of(&target),
                    claim_nonce: Nonce::from_bytes([1; 32]),
                    issued_at_ms: 1,
                    expires_at_ms: 1_000,
                },
                &target,
            )?;
            let request = Signed::sign(
                CrossNodeRecoveryRequest {
                    version: AUTH_VERSION,
                    account,
                    space,
                    source_node_public_key: PublicKey::of(&source),
                    saved_pins,
                    new_root_public_key: PublicKey::of(&root),
                    new_device_public_key: PublicKey::of(&client),
                    target_claim: claim.clone(),
                    source_read_resources: Vec::new(),
                    source_read_expires_at_ms: 2_000,
                    nonce: Nonce::from_bytes([2; 32]),
                    issued_at_ms: 1,
                    expires_at_ms: 1_000,
                },
                &recovery,
            )?;
            let snapshot = Signed::sign(
                RecoveryContextSnapshot {
                    version: AUTH_VERSION,
                    kind: RecoveryContextKind::CrossNode,
                    request_hash: request.hash()?,
                    snapshot_id: Nonce::from_bytes([3; 32]),
                    saved_pins_hash: RecoveryReadRequest::CrossNode(request.clone())
                        .saved_pins_hash()?,
                    target_heads,
                    source_node_public_key: PublicKey::of(&source),
                    claim,
                    issued_at_ms: 2,
                    expires_at_ms: 1_000,
                },
                &source,
            )?;
            Ok(Self {
                source,
                target,
                recovery,
                request,
                snapshot,
            })
        }

        fn cache(&self) -> Result<UntrustedTargetRecoveryCache> {
            UntrustedTargetRecoveryCache::new(&self.request, &self.snapshot.payload.snapshot_id)
        }

        fn page(
            &self,
            index: u64,
            previous: ObjectHash,
            complete: bool,
        ) -> Result<Signed<RecoveryContextPage>> {
            Ok(Signed::sign(
                RecoveryContextPage {
                    version: AUTH_VERSION,
                    kind: RecoveryContextKind::CrossNode,
                    request_hash: self.request.hash()?,
                    snapshot_id: self.snapshot.payload.snapshot_id.clone(),
                    target_heads_hash: recovery_target_heads_hash(
                        &self.snapshot.payload.target_heads,
                    )?,
                    page_index: index,
                    previous_page_hash: previous,
                    entry: None,
                    next_cursor: (!complete).then(|| ObjectHash::body(&index)).transpose()?,
                    complete,
                },
                &self.source,
            )?)
        }

        fn witness(
            &self,
            final_page: &Signed<RecoveryContextPage>,
        ) -> Result<Signed<RecoveryContextWitness>> {
            Ok(Signed::sign(
                RecoveryContextWitness {
                    version: AUTH_VERSION,
                    kind: RecoveryContextKind::CrossNode,
                    request_hash: self.request.hash()?,
                    snapshot_id: self.snapshot.payload.snapshot_id.clone(),
                    target_heads: self.snapshot.payload.target_heads.clone(),
                    final_page_hash: final_page.hash()?,
                    page_count: final_page.payload.page_index + 1,
                    entry_count: 0,
                    claim_hash: self.snapshot.payload.claim.hash()?,
                    source_node_public_key: PublicKey::of(&self.source),
                    issued_at_ms: 3,
                    expires_at_ms: 1_000,
                },
                &self.source,
            )?)
        }
    }

    fn approval_fixture(
        fixture: &Fixture,
        witness: &Signed<RecoveryContextWitness>,
    ) -> Result<UntrustedTargetRecoveryApproval> {
        let original = &fixture.request.payload;
        let account_head = fixture.snapshot.payload.target_heads[0].clone();
        let space_head = fixture.snapshot.payload.target_heads[1].clone();
        let account_recovery = Signed::sign(
            AccountDocument {
                version: AUTH_VERSION,
                account: original.account.clone(),
                genesis_root: original.saved_pins[0].genesis_root.clone(),
                current_root: original.new_root_public_key.clone(),
                root_generation: 1,
                previous: Some(account_head.clone()),
                authority_epoch: 1,
                sequence: 0,
                authority: AuthorityBinding::from_key(PublicKey::of(&fixture.target)),
                recovery: RecoveryPolicy {
                    keys: vec![fixture.request.issuer.clone()],
                    threshold: 1,
                },
                enrollment: Vec::new(),
                devices: Vec::new(),
                revoked_devices: Vec::new(),
                authenticators: Vec::new(),
                authenticator_approval: None,
                device_enrollment: None,
                root_acceptance: None,
                recovery_approvals: Vec::new(),
            },
            &fixture.recovery,
        )?;
        let node_certificate = DeviceCertificate {
            version: AUTH_VERSION,
            owner: PolicyDomain::Account(original.account.clone()),
            subject: Actor::Service(ServiceId::from_key(&PublicKey::of(&fixture.target))),
            device: DeviceId::from_key(&PublicKey::of(&fixture.target)),
            public_key: PublicKey::of(&fixture.target),
            uses: vec![DeviceUse::Noise, DeviceUse::ServiceInvocation],
            possession: None,
            enrollment_grant: None,
            issued_under: account_head.clone(),
            nonce: original.nonce.clone(),
            not_before_ms: 3,
            expires_at_ms: 2_000,
        };
        let proof = Signature::parse(
            &fixture
                .target
                .sign(
                    "device-possession-v2",
                    &node_certificate.possession_bytes()?,
                )
                .map_err(anyhow::Error::msg)?,
        )?;
        let mut source_certificate = node_certificate.clone();
        source_certificate.subject =
            Actor::Service(ServiceId::from_key(&PublicKey::of(&fixture.source)));
        source_certificate.device = DeviceId::from_key(&PublicKey::of(&fixture.source));
        source_certificate.public_key = PublicKey::of(&fixture.source);
        let context = RecoveryContextReference {
            snapshot_id: fixture.snapshot.payload.snapshot_id.clone(),
            request_hash: fixture.request.hash()?,
            snapshot_hash: fixture.snapshot.hash()?,
            witness_hash: witness.hash()?,
        };
        let possession = StreamedCrossNodeRecoveryPossessionRequest {
            context: context.clone(),
            request: fixture.request.clone(),
            node_request: RecoveryNodeRequest {
                claim: original.target_claim.clone(),
                expected_account_head: account_head,
                expected_space_head: space_head,
                account_recovery,
                node_certificate,
            },
            source_certificate,
        };
        let approval = Signed::sign(
            CrossNodeRecoveryImportApproval {
                version: AUTH_VERSION,
                context,
                account: original.account.clone(),
                space: original.space.clone(),
                new_device_public_key: original.new_device_public_key.clone(),
                new_root_public_key: original.new_root_public_key.clone(),
                source_node_public_key: PublicKey::of(&fixture.source),
                target_node_public_key: PublicKey::of(&fixture.target),
                possession_hash: ObjectHash::body(&possession)?,
                claim_hash: original.target_claim.hash()?,
                approved_at_ms: 4,
                expires_at_ms: original.expires_at_ms,
            },
            &fixture.target,
        )?;
        Ok(UntrustedTargetRecoveryApproval {
            possession,
            proof,
            approval,
        })
    }

    fn committed_fixture(
        fixture: &Fixture,
        approval: &UntrustedTargetRecoveryApproval,
    ) -> Result<StreamedCrossNodeRecoveryCommitted> {
        let node = &approval.possession.node_request;
        let request = StreamedCrossNodeRecoveryCommitRequest {
            possession: approval.possession.clone(),
            pins: fixture.request.payload.saved_pins.clone(),
            journal_delta: Vec::new(),
            grant_leaf: ObjectHash::body(&"recovered grant")?,
        };
        let receipt = Signed::sign(
            StreamedCrossNodeRecoveryCommitReceipt {
                version: AUTH_VERSION,
                context: approval.possession.context.clone(),
                request_hash: fixture.request.hash()?,
                input_hash: ObjectHash::body(&request)?,
                expected_account_head: node.expected_account_head.clone(),
                expected_space_head: node.expected_space_head.clone(),
                committed_account_head: node.expected_account_head.clone(),
                committed_space_head: node.expected_space_head.clone(),
                target_node_public_key: PublicKey::of(&fixture.target),
                new_device_public_key: fixture.request.payload.new_device_public_key.clone(),
                grant_leaf: request.grant_leaf.clone(),
                accepted_at_ms: 5,
            },
            &fixture.source,
        )?;
        Ok(StreamedCrossNodeRecoveryCommitted { request, receipt })
    }

    fn database() -> Result<Connection> {
        let connection = Connection::open_in_memory()?;
        connection.execute_batch(
            "CREATE TABLE identity_meta(key TEXT PRIMARY KEY,value TEXT NOT NULL);",
        )?;
        Ok(connection)
    }

    #[test]
    fn retries_preserve_complete_signed_carriers_in_separate_rows() -> Result<()> {
        let fixture = Fixture::new()?;
        let cache = fixture.cache()?;
        let mut connection = database()?;
        let transaction = connection.transaction()?;
        cache.store_header(&transaction, &fixture.snapshot)?;
        cache.store_header(&transaction, &fixture.snapshot)?;
        let first = fixture.page(0, fixture.snapshot.hash()?, false)?;
        let final_page = fixture.page(1, first.hash()?, true)?;
        cache.store_page(&transaction, &first)?;
        cache.store_page(&transaction, &first)?;
        cache.store_page(&transaction, &final_page)?;
        let witness = fixture.witness(&final_page)?;
        cache.store_witness(&transaction, &witness)?;
        cache.store_witness(&transaction, &witness)?;
        transaction.commit()?;
        assert_eq!(cache.load_page(&connection, 0)?, Some(first.clone()));
        assert_eq!(cache.load_page(&connection, 1)?, Some(final_page));
        assert_eq!(cache.load_witness(&connection)?, Some(witness));
        let rows: u64 =
            connection.query_row("SELECT count(*) FROM identity_meta", [], |row| row.get(0))?;
        assert_eq!(rows, 4);
        let mut changed = first.payload.clone();
        changed.next_cursor = Some(ObjectHash::body(&"different cursor")?);
        let changed = Signed::sign(changed, &fixture.source)?;
        assert!(cache.store_page(&connection, &changed).is_err());
        assert_ne!(cache.load_page(&connection, 0)?, Some(changed));
        let mut changed_signature = first.clone();
        changed_signature.signature = fixture.snapshot.signature.clone();
        assert_eq!(first.hash()?, changed_signature.hash()?);
        connection.execute(
            "UPDATE identity_meta SET value=? WHERE key=?",
            params![
                serde_json::to_string(&changed_signature)?,
                cache.page_key(0)
            ],
        )?;
        assert!(cache.store_page(&connection, &first).is_err());
        Ok(())
    }

    #[test]
    fn mixed_scope_skipped_pages_and_pages_after_completion_are_rejected() -> Result<()> {
        let fixture = Fixture::new()?;
        let cache = fixture.cache()?;
        let connection = database()?;
        cache.store_header(&connection, &fixture.snapshot)?;
        let other = Fixture::new()?;
        assert!(cache.store_header(&connection, &other.snapshot).is_err());
        let other_page = other.page(0, other.snapshot.hash()?, true)?;
        assert!(cache.store_page(&connection, &other_page).is_err());
        let skipped = fixture.page(1, fixture.snapshot.hash()?, true)?;
        assert!(cache.store_page(&connection, &skipped).is_err());
        assert!(
            cache
                .store_witness(&connection, &fixture.witness(&skipped)?)
                .is_err()
        );
        let wrong_start = fixture.page(0, ObjectHash::body(&"wrong previous")?, true)?;
        assert!(cache.store_page(&connection, &wrong_start).is_err());
        let first = fixture.page(0, fixture.snapshot.hash()?, true)?;
        cache.store_page(&connection, &first)?;
        let after_complete = fixture.page(1, first.hash()?, true)?;
        assert!(cache.store_page(&connection, &after_complete).is_err());
        let mut witness = fixture.witness(&first)?.payload;
        witness.final_page_hash = ObjectHash::body(&"different final page")?;
        let witness = Signed::sign(witness, &fixture.source)?;
        assert!(cache.store_witness(&connection, &witness).is_err());
        let mut changed = first.payload;
        changed.target_heads_hash = ObjectHash::body(&"other heads")?;
        let changed = Signed::sign(changed, &fixture.source)?;
        assert!(cache.store_page(&connection, &changed).is_err());
        assert!(other.cache()?.load_header(&connection)?.is_none());
        Ok(())
    }

    #[test]
    fn oversized_row_is_rejected_without_reading_other_pages() -> Result<()> {
        let fixture = Fixture::new()?;
        let cache = fixture.cache()?;
        let connection = database()?;
        cache.store_header(&connection, &fixture.snapshot)?;
        let first = fixture.page(0, fixture.snapshot.hash()?, false)?;
        let final_page = fixture.page(1, first.hash()?, true)?;
        cache.store_page(&connection, &first)?;
        cache.store_page(&connection, &final_page)?;
        let mut tampered = first.clone();
        tampered.payload.complete = true;
        tampered.payload.next_cursor = None;
        connection.execute(
            "UPDATE identity_meta SET value=? WHERE key=?",
            params![serde_json::to_string(&tampered)?, cache.page_key(0)],
        )?;
        assert!(cache.load_page(&connection, 0).is_err());
        assert_eq!(cache.load_page(&connection, 1)?, Some(final_page.clone()));
        let mut oversized = serde_json::to_string(&first)?;
        oversized.push_str(&" ".repeat(MAX_CACHE_ROW_BYTES + 1 - oversized.len()));
        assert_eq!(
            serde_json::from_str::<Signed<RecoveryContextPage>>(&oversized)?,
            first
        );
        connection.execute(
            "UPDATE identity_meta SET value=? WHERE key=?",
            params![oversized, cache.page_key(0)],
        )?;
        assert!(cache.load_page(&connection, 0).is_err());
        assert_eq!(cache.load_page(&connection, 1)?, Some(final_page));
        Ok(())
    }

    #[test]
    fn approval_and_completion_preserve_exact_original_carriers() -> Result<()> {
        let fixture = Fixture::new()?;
        let cache = fixture.cache()?;
        let connection = database()?;
        cache.store_header(&connection, &fixture.snapshot)?;
        let page = fixture.page(0, fixture.snapshot.hash()?, true)?;
        cache.store_page(&connection, &page)?;
        let witness = fixture.witness(&page)?;
        cache.store_witness(&connection, &witness)?;
        let approval = approval_fixture(&fixture, &witness)?;
        let committed = committed_fixture(&fixture, &approval)?;
        assert!(cache.store_committed(&connection, &committed).is_err());
        let mut wrong_approval = approval.clone();
        wrong_approval.approval.payload.possession_hash = ObjectHash::body(&"other possession")?;
        wrong_approval.approval = Signed::sign(wrong_approval.approval.payload, &fixture.target)?;
        assert!(cache.store_approval(&connection, &wrong_approval).is_err());
        let mut wrong_context = approval.clone();
        wrong_context.possession.context.witness_hash = ObjectHash::body(&"other witness")?;
        wrong_context.approval.payload.context = wrong_context.possession.context.clone();
        wrong_context.approval.payload.possession_hash =
            ObjectHash::body(&wrong_context.possession)?;
        wrong_context.approval = Signed::sign(wrong_context.approval.payload, &fixture.target)?;
        assert!(cache.store_approval(&connection, &wrong_context).is_err());
        assert!(cache.load_approval(&connection)?.is_none());

        cache.store_approval(&connection, &approval)?;
        cache.store_approval(&connection, &approval)?;
        assert_eq!(cache.load_approval(&connection)?, Some(approval.clone()));
        let mut changed_proof = approval.clone();
        changed_proof.proof = fixture.snapshot.signature.clone();
        assert!(cache.store_approval(&connection, &changed_proof).is_err());

        let mut changed_input = committed.clone();
        changed_input.request.pins.reverse();
        assert!(cache.store_committed(&connection, &changed_input).is_err());
        let mut wrong_client = committed.clone();
        wrong_client.receipt.payload.new_device_public_key = PublicKey::of(&fixture.source);
        wrong_client.receipt = Signed::sign(wrong_client.receipt.payload, &fixture.source)?;
        assert!(cache.store_committed(&connection, &wrong_client).is_err());
        assert!(cache.load_committed(&connection)?.is_none());
        cache.store_committed(&connection, &committed)?;
        cache.store_committed(&connection, &committed)?;
        assert_eq!(cache.load_committed(&connection)?, Some(committed));
        Ok(())
    }
}
