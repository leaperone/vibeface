//! Small durable manifests; incomplete bytes never enter the replicated blob table.
use super::*;
use serde::{Deserialize, Serialize};

pub(crate) const PREFIX: &str = "net1:v1:blob:";

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct BlobTransfer {
    pub version: u32,
    pub blob: BlobRef,
    pub phase: TransferPhase,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum TransferPhase {
    Receiving {
        committed_offset: u64,
        source_node_id: String,
    },
    Cleanup,
}

impl BlobTransfer {
    pub fn key(blob: &BlobRef) -> Result<String> {
        ensure!(
            blob.hash.len() == 64
                && blob
                    .hash
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
                && blob.size <= MAX_BLOB_BYTES as u64,
            "invalid blob reference"
        );
        Ok(format!("{PREFIX}{}:{}", blob.hash, blob.size))
    }
    pub fn receiving(blob: &BlobRef, source: &str, offset: u64) -> Self {
        Self {
            version: 1,
            blob: blob.clone(),
            phase: TransferPhase::Receiving {
                committed_offset: offset,
                source_node_id: source.into(),
            },
        }
    }
    pub fn validate(&self, key: &str) -> Result<()> {
        ensure!(
            self.version == 1 && Self::key(&self.blob)? == key,
            "invalid blob transfer manifest"
        );
        if let TransferPhase::Receiving {
            committed_offset,
            source_node_id,
        } = &self.phase
        {
            ensure!(
                *committed_offset <= self.blob.size
                    && !source_node_id.is_empty()
                    && source_node_id.len() <= 128
                    && !source_node_id.chars().any(char::is_control),
                "invalid receiving progress"
            );
        }
        Ok(())
    }
}

fn read_from(
    connection: &Connection,
    network: &str,
    blob: &BlobRef,
) -> Result<Option<(BlobTransfer, String)>> {
    let key = BlobTransfer::key(blob)?;
    let raw: Option<String> = connection
        .query_row(
            "SELECT value FROM local_values WHERE network_id=? AND key=?",
            params![network, key],
            |row| row.get(0),
        )
        .optional()?;
    raw.map(|raw| {
        let transfer: BlobTransfer = serde_json::from_str(&raw)?;
        transfer.validate(&key)?;
        ensure!(transfer.blob == *blob, "blob transfer object mismatch");
        Ok((transfer, raw))
    })
    .transpose()
}

fn update_from(
    connection: &Connection,
    network: &str,
    previous_raw: &str,
    transfer: &BlobTransfer,
) -> Result<()> {
    let key = BlobTransfer::key(&transfer.blob)?;
    transfer.validate(&key)?;
    ensure!(
        connection.execute(
            "UPDATE local_values SET value=? WHERE network_id=? AND key=? AND value=?",
            params![serde_json::to_string(transfer)?, network, key, previous_raw],
        )? == 1,
        "blob transfer changed concurrently"
    );
    Ok(())
}

impl Store {
    pub(crate) fn blob_transfer(
        &self,
        network: &str,
        blob: &BlobRef,
    ) -> Result<Option<BlobTransfer>> {
        Ok(read_from(&self.connection, network, blob)?.map(|(transfer, _)| transfer))
    }

    pub(crate) fn blob_transfers(
        &self,
        after: Option<(&str, &str)>,
        limit: usize,
    ) -> Result<Vec<(String, String, Vec<u8>)>> {
        let (network, key) = after.unwrap_or(("", ""));
        let mut statement = self.connection.prepare(
            "SELECT network_id,key,CAST(value AS BLOB) FROM local_values WHERE key>=? AND key<? AND (network_id>? OR (network_id=? AND key>?)) ORDER BY network_id,key LIMIT ?",
        )?;
        let rows = statement.query_map(
            params![
                PREFIX,
                "net1:v1:blob;",
                network,
                network,
                key,
                limit.clamp(1, 65)
            ],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub(crate) fn advance_blob_transfer(
        &mut self,
        network: &str,
        local_node: &str,
        previous: &BlobTransfer,
        next: &BlobTransfer,
    ) -> Result<()> {
        let TransferPhase::Receiving { source_node_id, .. } = &next.phase else {
            bail!("receiving progress required");
        };
        ensure!(
            previous.blob == next.blob && matches!(previous.phase, TransferPhase::Receiving { .. }),
            "invalid blob transfer transition"
        );
        next.validate(&BlobTransfer::key(&next.blob)?)?;
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        authorize_node_from(&tx, network, local_node, Some(NodeRole::Replica))?;
        authorize_node_from(&tx, network, source_node_id, Some(NodeRole::Replica))?;
        let (current, raw) =
            read_from(&tx, network, &next.blob)?.context("blob transfer disappeared")?;
        ensure!(current == *previous, "blob transfer changed concurrently");
        update_from(&tx, network, &raw, next)?;
        tx.commit()?;
        Ok(())
    }

    pub(crate) fn publish_blob_transfer(
        &mut self,
        network: &str,
        local_node: &str,
        expected: &BlobRef,
        source: &str,
        bytes: &[u8],
    ) -> Result<bool> {
        ensure!(
            bytes.len() as u64 == expected.size
                && bytes.len() <= MAX_BLOB_BYTES
                && digest(bytes) == expected.hash,
            "blob size/hash mismatch"
        );
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        authorize_node_from(&tx, network, local_node, Some(NodeRole::Replica))?;
        authorize_node_from(&tx, network, source, Some(NodeRole::Replica))?;
        let (mut transfer, raw) =
            read_from(&tx, network, expected)?.context("blob transfer disappeared")?;
        ensure!(
            transfer.phase
                == TransferPhase::Receiving {
                    committed_offset: expected.size,
                    source_node_id: source.into()
                },
            "blob transfer is not ready for publication"
        );
        let changed = !has_blob_from(&tx, network, expected)?;
        if changed {
            self.blob_readers.retire_blob(network, &expected.hash);
            tx.execute("INSERT INTO blobs(network_id,hash,size,content) VALUES(?,?,?,?) ON CONFLICT(network_id,hash) DO UPDATE SET size=excluded.size,content=excluded.content",
                params![network,expected.hash,expected.size,bytes])?;
            rebuild_projections_from(&tx, network)?;
        }
        transfer.phase = TransferPhase::Cleanup;
        update_from(&tx, network, &raw, &transfer)?;
        tx.commit()?;
        Ok(changed)
    }

    pub(crate) fn prepare_blob_cleanup(
        &mut self,
        network: &str,
        expected: &BlobRef,
    ) -> Result<bool> {
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let Some((mut transfer, raw)) = read_from(&tx, network, expected)? else {
            return Ok(false);
        };
        let complete = get_blob_for_ref_from(&tx, network, expected)?;
        if complete.is_none() {
            ensure!(
                transfer.phase != TransferPhase::Cleanup,
                "published blob missing; preserving staging copy"
            );
            return Ok(false);
        }
        if transfer.phase != TransferPhase::Cleanup {
            transfer.phase = TransferPhase::Cleanup;
            update_from(&tx, network, &raw, &transfer)?;
        }
        tx.commit()?;
        Ok(true)
    }

    pub(crate) fn delete_blob_cleanup(&mut self, network: &str, expected: &BlobRef) -> Result<()> {
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        if let Some((transfer, raw)) = read_from(&tx, network, expected)? {
            ensure!(
                transfer.phase == TransferPhase::Cleanup,
                "blob transfer is not cleanup"
            );
            ensure!(
                tx.execute(
                    "DELETE FROM local_values WHERE network_id=? AND key=? AND value=?",
                    params![network, BlobTransfer::key(expected)?, raw]
                )? == 1,
                "blob cleanup changed concurrently"
            );
        }
        tx.commit()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn publication_and_cleanup_are_atomic_and_cleanup_is_local() -> Result<()> {
        let owner = LocalIdentity::generate();
        let network = owner.fixture_network_id()?;
        let node = owner.peer_id()?;
        let mut store = Store::open(":memory:")?;
        store.install_trust_bundle(
            &network,
            &owner.sign_network_fixture(1, vec![owner.certificate(vec![NodeRole::Replica])?])?,
        )?;
        let bytes = b"transactional blob";
        let blob = BlobRef {
            hash: digest(bytes),
            size: bytes.len() as u64,
        };
        let manifest = BlobTransfer::receiving(&blob, &node, blob.size);
        let key = BlobTransfer::key(&blob)?;
        store.local_set(&network, &key, &serde_json::to_value(&manifest)?)?;
        let generation = store.projection_generation(&network)?;
        store.connection.execute_batch("CREATE TRIGGER reject_cleanup BEFORE UPDATE ON local_values WHEN json_extract(NEW.value,'$.phase.state')='cleanup' BEGIN SELECT RAISE(ABORT,'test cleanup failure'); END;")?;
        assert!(
            store
                .publish_blob_transfer(&network, &node, &blob, &node, bytes)
                .is_err()
        );
        assert!(store.get_blob(&network, &blob.hash)?.is_none());
        assert_eq!(
            store.blob_transfer(&network, &blob)?,
            Some(manifest.clone())
        );
        assert_eq!(store.projection_generation(&network)?, generation);
        store
            .connection
            .execute_batch("DROP TRIGGER reject_cleanup;")?;
        assert!(store.publish_blob_transfer(&network, &node, &blob, &node, bytes)?);
        assert_eq!(
            store.blob_transfer(&network, &blob)?.unwrap().phase,
            TransferPhase::Cleanup
        );
        assert_eq!(store.projection_generation(&network)?, generation + 1);
        // A second Receiving can be cleared after an independent valid put_blob, without old-source authorization.
        let mut receiving = manifest;
        receiving.phase = TransferPhase::Receiving {
            committed_offset: 0,
            source_node_id: "revoked-source".into(),
        };
        store.local_set(&network, &key, &serde_json::to_value(receiving)?)?;
        let mut revoked = owner.certificate(vec![NodeRole::Replica])?;
        revoked.revoked = true;
        store.install_trust_bundle(&network, &owner.sign_network_fixture(2, vec![revoked])?)?;
        assert!(
            store
                .authorize_node(&network, &node, Some(NodeRole::Replica))
                .is_err()
        );
        let revoked_generation = store.projection_generation(&network)?;
        assert!(store.prepare_blob_cleanup(&network, &blob)?);
        assert_eq!(store.get_blob(&network, &blob.hash)?.unwrap(), bytes);
        assert_eq!(store.projection_generation(&network)?, revoked_generation);
        // Cleanup phase alone never authorizes unlinking the only correct staging copy.
        store.connection.execute(
            "UPDATE blobs SET content=? WHERE network_id=? AND hash=?",
            params![vec![0_u8; bytes.len()], network, blob.hash],
        )?;
        assert!(store.prepare_blob_cleanup(&network, &blob).is_err());
        assert_eq!(
            store.blob_transfer(&network, &blob)?.unwrap().phase,
            TransferPhase::Cleanup
        );
        store.connection.execute(
            "DELETE FROM blobs WHERE network_id=? AND hash=?",
            params![network, blob.hash],
        )?;
        assert!(store.prepare_blob_cleanup(&network, &blob).is_err());
        assert!(store.blob_transfer(&network, &blob)?.is_some());
        Ok(())
    }

    #[test]
    fn transfer_enumeration_preserves_bad_rows_and_exact_object_boundaries() -> Result<()> {
        let mut store = Store::open(":memory:")?;
        let blob = BlobRef {
            hash: digest(b"one"),
            size: 3,
        };
        let key = BlobTransfer::key(&blob)?;
        store.connection.execute(
            "INSERT INTO local_values(network_id,key,value) VALUES('a',?,'not-json')",
            [&key],
        )?;
        let other = BlobRef {
            hash: blob.hash.clone(),
            size: 4,
        };
        store.local_set(
            "b",
            &BlobTransfer::key(&other)?,
            &serde_json::to_value(BlobTransfer::receiving(&other, "source", 0))?,
        )?;
        let first = store.blob_transfers(None, 1)?;
        assert_eq!(first[0], ("a".into(), key.clone(), b"not-json".to_vec()));
        let second = store.blob_transfers(Some((&first[0].0, &first[0].1)), 1)?;
        assert_eq!(second[0].0, "b");
        assert!(store.blob_transfer("a", &blob).is_err());
        assert!(store.blob_transfer("b", &blob)?.is_none());
        assert_eq!(store.blob_transfer("b", &other)?.unwrap().blob, other);
        Ok(())
    }
}
