use anyhow::{Context, Result, ensure};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use vibeface_secure::authorization::{
    AUTH_VERSION, Nonce, ObjectHash, Signed,
    bootstrap::recovery::{cross_node::CrossNodeRecoveryRequest, stream::*},
};

const MAX_CACHE_ROW_BYTES: usize = 65_536;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct UntrustedTargetRecoveryHeader {
    pub(crate) request: RecoveryReadRequest,
    pub(crate) snapshot: Signed<RecoveryContextSnapshot>,
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

    fn header_key(&self) -> String {
        format!("{}:header", self.scope_key)
    }

    fn page_key(&self, index: u64) -> String {
        format!("{}:page:{index}", self.scope_key)
    }

    fn witness_key(&self) -> String {
        format!("{}:witness", self.scope_key)
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
            AccountId, PolicyDomain, PolicyPin, PolicyStamp, PublicKey, SpaceId,
            bootstrap::NodeClaim,
        },
    };

    struct Fixture {
        source: Identity,
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
}
