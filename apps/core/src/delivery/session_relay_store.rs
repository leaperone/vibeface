use super::session::SessionStoredReceipt;
use anyhow::{Context, Result, bail, ensure};
use base64::{Engine, engine::general_purpose::STANDARD};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::Path;
use vibeface_delivery::{
    CAPSULE_BYTES, EncryptedSessionObject, MAX_SESSION_OBJECT_BYTES, PeerSignature,
    SessionObjectHeader, Word, hash,
};

pub(super) const RELAY_CHUNK_BYTES: usize = 8192;
pub(super) const SESSION_DOWNLOAD_DB: &str = "delivery/session-downloads.sqlite3";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct RelayManifest {
    pub header: SessionObjectHeader,
    pub signature: PeerSignature,
    pub capsule: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct RelayProgress {
    pub committed: u64,
    pub complete: bool,
}

pub(super) struct SessionRelayStore {
    db: Connection,
    downloads: bool,
}

impl SessionRelayStore {
    pub(super) fn open(data_dir: &Path) -> Result<Self> {
        Self::open_at(data_dir, "delivery/session-relay.sqlite3", false)
    }

    pub(super) fn open_downloads(data_dir: &Path) -> Result<Self> {
        Self::open_at(data_dir, SESSION_DOWNLOAD_DB, true)
    }

    fn open_at(data_dir: &Path, relative_path: &str, downloads: bool) -> Result<Self> {
        let mut db = crate::store::private_file_connection(&data_dir.join(relative_path))?;
        db.busy_timeout(std::time::Duration::from_secs(5))?;
        db.execute_batch(
            "PRAGMA foreign_keys=ON; PRAGMA journal_mode=DELETE; PRAGMA synchronous=FULL;
             PRAGMA secure_delete=ON; PRAGMA auto_vacuum=FULL;",
        )?;
        let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let version: u32 = tx.pragma_query_value(None, "user_version", |row| row.get(0))?;
        match version {
            0 => {
                let objects: u32 = tx.query_row(
                    "SELECT count(*) FROM sqlite_schema WHERE name NOT LIKE 'sqlite_%'",
                    [],
                    |row| row.get(0),
                )?;
                ensure!(
                    objects == 0,
                    "unversioned session relay database is unsupported"
                );
                tx.execute_batch(
                    "CREATE TABLE objects (
                        id BLOB PRIMARY KEY NOT NULL,
                        sender BLOB NOT NULL,
                        receiver BLOB NOT NULL,
                        manifest TEXT NOT NULL,
                        size INTEGER NOT NULL CHECK (size BETWEEN 64 AND 16777216),
                        expires INTEGER NOT NULL,
                        committed INTEGER NOT NULL DEFAULT 0 CHECK (committed BETWEEN 0 AND size),
                        complete INTEGER NOT NULL DEFAULT 0 CHECK (complete IN (0,1)),
                        receipt TEXT,
                        CHECK (complete=0 OR committed=size)
                    );
                    CREATE INDEX objects_expiry ON objects(expires);
                    CREATE TABLE chunks (
                        id BLOB NOT NULL REFERENCES objects(id) ON DELETE CASCADE,
                        offset INTEGER NOT NULL CHECK (offset>=0),
                        bytes BLOB NOT NULL CHECK (length(bytes) BETWEEN 1 AND 8192),
                        PRIMARY KEY(id,offset)
                    ) WITHOUT ROWID;",
                )?;
                tx.pragma_update(None, "user_version", crate::config::DATA_EPOCH)?;
            }
            crate::config::DATA_EPOCH => {}
            _ => bail!(
                "unsupported session relay database schema version {version}; initialize a fresh Node data directory"
            ),
        }
        tx.commit()?;
        Ok(Self { db, downloads })
    }

    pub(super) fn begin(
        &mut self,
        manifest: &RelayManifest,
        now_ms: u64,
        capacity: u64,
        max_orders: u32,
    ) -> Result<RelayProgress> {
        self.prune(now_ms)?;
        let header = &manifest.header;
        header.canonical_bytes()?;
        ensure!(
            header.created_at_ms <= now_ms && now_ms < header.expires_at_ms,
            "session relay manifest is outside its validity interval"
        );
        ensure!(
            (64..=MAX_SESSION_OBJECT_BYTES as u64).contains(&header.payload_bytes)
                && u64::from(header.chunk_count)
                    == header.payload_bytes.div_ceil(RELAY_CHUNK_BYTES as u64),
            "invalid session relay payload size or chunk count"
        );
        let manifest_json = serde_json::to_string(manifest)?;
        let tx = self
            .db
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let existing: Option<(String, u64, bool)> = tx
            .query_row(
                "SELECT manifest,committed,complete FROM objects WHERE id=?1",
                [header.object_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()?;
        if let Some((prior, committed, complete)) = existing {
            ensure!(
                prior == manifest_json,
                "session relay manifest changed across retry"
            );
            return Ok(RelayProgress {
                committed,
                complete,
            });
        }
        let (reserved, orders): (u64, u32) = tx.query_row(
            "SELECT coalesce(sum(size),0),count(*) FROM objects",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        ensure!(
            orders < max_orders
                && reserved
                    .checked_add(header.payload_bytes)
                    .is_some_and(|size| size <= capacity),
            "session relay capacity is exhausted"
        );
        tx.execute(
            "INSERT INTO objects(id,sender,receiver,manifest,size,expires) VALUES(?1,?2,?3,?4,?5,?6)",
            params![header.object_id, header.sender_peer, header.receiver_peer, manifest_json,
                header.payload_bytes, header.expires_at_ms],
        )?;
        tx.commit()?;
        Ok(RelayProgress {
            committed: 0,
            complete: false,
        })
    }

    pub(super) fn put(
        &mut self,
        id: &Word,
        sender: &Word,
        offset: u64,
        bytes: &[u8],
        now_ms: u64,
    ) -> Result<RelayProgress> {
        self.prune(now_ms)?;
        let tx = self
            .db
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let (manifest_json, committed, complete): (String, u64, bool) = tx.query_row(
            "SELECT manifest,committed,complete FROM objects WHERE id=?1 AND sender=?2 AND expires>?3",
            params![id, sender, now_ms],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        ).optional()?.context("session relay object is unavailable for this sender")?;
        let manifest: RelayManifest = serde_json::from_str(&manifest_json)?;
        ensure!(
            offset < manifest.header.payload_bytes
                && offset.is_multiple_of(RELAY_CHUNK_BYTES as u64),
            "invalid session relay chunk offset"
        );
        ensure!(
            bytes.len() as u64
                == (manifest.header.payload_bytes - offset).min(RELAY_CHUNK_BYTES as u64),
            "invalid session relay chunk length"
        );
        let next = offset + bytes.len() as u64;
        if offset < committed {
            let prior: Vec<u8> = tx.query_row(
                "SELECT bytes FROM chunks WHERE id=?1 AND offset=?2",
                params![id, offset],
                |row| row.get(0),
            )?;
            ensure!(
                next <= committed && prior == bytes,
                "session relay chunk changed across retry"
            );
            return Ok(RelayProgress {
                committed,
                complete,
            });
        }
        ensure!(
            !complete && offset == committed,
            "session relay chunk is not at the durable prefix"
        );
        tx.execute(
            "INSERT INTO chunks(id,offset,bytes) VALUES(?1,?2,?3)",
            params![id, offset, bytes],
        )?;
        let complete = next == manifest.header.payload_bytes;
        if complete && !complete_payload_matches(&tx, id, &manifest)? {
            tx.execute("DELETE FROM objects WHERE id=?1", [id])?;
            tx.commit()?;
            bail!("session relay payload hash or capsule commitment mismatch");
        }
        tx.execute(
            "UPDATE objects SET committed=?2,complete=?3 WHERE id=?1",
            params![id, next, complete],
        )?;
        tx.commit()?;
        Ok(RelayProgress {
            committed: next,
            complete,
        })
    }

    pub(super) fn inbox(
        &self,
        receiver: &Word,
        sender: &Word,
        conversation: &str,
        now_ms: u64,
    ) -> Result<Vec<Word>> {
        self.prune(now_ms)?;
        let mut statement = self.db.prepare(
            "SELECT manifest FROM objects WHERE receiver=?1 AND sender=?2 AND complete=1
                AND receipt IS NULL AND expires>?3 ORDER BY rowid",
        )?;
        let mut rows = statement.query(params![receiver, sender, now_ms])?;
        let mut objects = Vec::new();
        while let Some(row) = rows.next()? {
            let manifest: RelayManifest = serde_json::from_str(&row.get::<_, String>(0)?)?;
            if manifest.header.kind != vibeface_delivery::SessionObjectKind::Attachment
                && manifest.header.conversation_id == conversation
            {
                objects.push(manifest.header.object_id);
                if objects.len() == 16 {
                    break;
                }
            }
        }
        Ok(objects)
    }

    pub(super) fn manifest(
        &self,
        id: &Word,
        receiver: &Word,
        now_ms: u64,
    ) -> Result<RelayManifest> {
        self.prune(now_ms)?;
        let json: String = self.db.query_row(
            "SELECT manifest FROM objects WHERE id=?1 AND receiver=?2 AND complete=1 AND expires>?3",
            params![id, receiver, now_ms],
            |row| row.get(0),
        ).optional()?.context("complete session relay object is unavailable for this receiver")?;
        Ok(serde_json::from_str(&json)?)
    }

    pub(super) fn get(
        &self,
        id: &Word,
        receiver: &Word,
        offset: u64,
        now_ms: u64,
    ) -> Result<Vec<u8>> {
        let manifest = self.manifest(id, receiver, now_ms)?;
        ensure!(
            offset < manifest.header.payload_bytes
                && offset.is_multiple_of(RELAY_CHUNK_BYTES as u64),
            "invalid session relay chunk offset"
        );
        Ok(self.db.query_row(
            "SELECT bytes FROM chunks WHERE id=?1 AND offset=?2",
            params![id, offset],
            |row| row.get(0),
        )?)
    }

    pub(super) fn complete_object(
        &self,
        id: &Word,
        receiver: &Word,
        now_ms: u64,
    ) -> Result<EncryptedSessionObject> {
        self.ensure_download_store()?;
        self.prune(now_ms)?;
        let (manifest_json, size, committed): (String, u64, u64) = self
            .db
            .query_row(
                "SELECT manifest,size,committed FROM objects
                 WHERE id=?1 AND receiver=?2 AND complete=1 AND expires>?3",
                params![id, receiver, now_ms],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()?
            .context("complete session download is unavailable for this receiver")?;
        let manifest: RelayManifest = serde_json::from_str(&manifest_json)?;
        let header = &manifest.header;
        ensure!(
            header.object_id == *id,
            "session download object id mismatch"
        );
        ensure!(
            header.receiver_peer == *receiver,
            "session download is unavailable for this receiver"
        );
        header.canonical_bytes()?;
        ensure!(
            (64..=MAX_SESSION_OBJECT_BYTES as u64).contains(&header.payload_bytes)
                && u64::from(header.chunk_count)
                    == header.payload_bytes.div_ceil(RELAY_CHUNK_BYTES as u64),
            "invalid session download payload size or chunk count"
        );
        ensure!(
            size == header.payload_bytes && committed == size,
            "session download extent does not match its manifest"
        );
        let expected_chunks = usize::try_from(header.chunk_count)?;
        let mut ciphertext = Vec::with_capacity(usize::try_from(header.payload_bytes)?);
        let mut stored = 0u64;
        let mut chunks = 0usize;
        let mut statement = self
            .db
            .prepare("SELECT offset,bytes FROM chunks WHERE id=?1 ORDER BY offset LIMIT ?2")?;
        let mut rows = statement.query(params![id, expected_chunks.saturating_add(1)])?;
        while let Some(row) = rows.next()? {
            let offset: u64 = row.get(0)?;
            let bytes: Vec<u8> = row.get(1)?;
            ensure!(
                chunks < expected_chunks
                    && offset == stored
                    && offset.is_multiple_of(RELAY_CHUNK_BYTES as u64)
                    && !bytes.is_empty()
                    && bytes.len()
                        == (header.payload_bytes - offset).min(RELAY_CHUNK_BYTES as u64) as usize,
                "session download chunks are not a bounded contiguous sequence"
            );
            let next = stored
                .checked_add(bytes.len() as u64)
                .context("session download chunk extent overflow")?;
            ensure!(
                next <= header.payload_bytes,
                "session download chunk exceeds object"
            );
            ciphertext.extend_from_slice(&bytes);
            stored = next;
            chunks += 1;
        }
        ensure!(
            chunks == expected_chunks && stored == header.payload_bytes,
            "session download is missing chunks"
        );
        Ok(EncryptedSessionObject {
            header: manifest.header,
            ciphertext,
            signature: Some(manifest.signature),
        })
    }

    pub(super) fn discard_download(&mut self, id: &Word, receiver: &Word) -> Result<()> {
        self.ensure_download_store()?;
        let tx = self
            .db
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.execute(
            "DELETE FROM objects WHERE id=?1 AND receiver=?2",
            params![id, receiver],
        )?;
        tx.commit()?;
        Ok(())
    }

    pub(super) fn put_receipt(
        &mut self,
        id: &Word,
        receiver: &Word,
        receipt: &SessionStoredReceipt,
        now_ms: u64,
    ) -> Result<()> {
        self.prune(now_ms)?;
        let tx = self
            .db
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let prior: Option<String> = tx.query_row(
            "SELECT receipt FROM objects WHERE id=?1 AND receiver=?2 AND complete=1 AND expires>?3",
            params![id, receiver, now_ms],
            |row| row.get(0),
        ).optional()?.context("complete session relay object is unavailable for this receiver")?;
        let receipt_json = serde_json::to_string(receipt)?;
        if let Some(prior) = prior {
            ensure!(
                prior == receipt_json,
                "session relay receipt changed across retry"
            );
        } else {
            tx.execute(
                "UPDATE objects SET receipt=?2 WHERE id=?1",
                params![id, receipt_json],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    pub(super) fn receipt(
        &self,
        id: &Word,
        sender: &Word,
        now_ms: u64,
    ) -> Result<Option<SessionStoredReceipt>> {
        self.prune(now_ms)?;
        let json: Option<String> = self.db.query_row(
            "SELECT receipt FROM objects WHERE id=?1 AND sender=?2 AND complete=1 AND expires>?3",
            params![id, sender, now_ms],
            |row| row.get(0),
        ).optional()?.context("complete session relay object is unavailable for this sender")?;
        Ok(json.as_deref().map(serde_json::from_str).transpose()?)
    }

    pub(super) fn purge(&mut self, now_ms: u64) -> Result<()> {
        self.prune(now_ms)
    }

    fn prune(&self, now_ms: u64) -> Result<()> {
        self.db
            .execute("DELETE FROM objects WHERE expires<=?1", [now_ms])?;
        Ok(())
    }

    fn ensure_download_store(&self) -> Result<()> {
        ensure!(
            self.downloads,
            "session download operation requires the receiver store"
        );
        Ok(())
    }
}

fn complete_payload_matches(db: &Connection, id: &Word, manifest: &RelayManifest) -> Result<bool> {
    let Ok(capsule) = STANDARD.decode(&manifest.capsule) else {
        return Ok(false);
    };
    if capsule.len() != CAPSULE_BYTES {
        return Ok(false);
    }
    let mut digest = Sha256::new();
    let mut stored = 0;
    let mut capsule_commitment: Option<Word> = None;
    let mut chunks = db.prepare("SELECT offset,bytes FROM chunks WHERE id=?1 ORDER BY offset")?;
    let mut rows = chunks.query([id])?;
    while let Some(row) = rows.next()? {
        let offset: u64 = row.get(0)?;
        let bytes: Vec<u8> = row.get(1)?;
        if offset != stored {
            return Ok(false);
        }
        if stored == 0 {
            capsule_commitment = bytes.get(32..64).and_then(|bytes| bytes.try_into().ok());
        }
        stored += bytes.len() as u64;
        digest.update(&bytes);
    }
    let payload_hash: Word = digest.finalize().into();
    Ok(stored == manifest.header.payload_bytes
        && payload_hash == manifest.header.payload_hash
        && capsule_commitment == Some(hash(&capsule)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use vibeface_delivery::{EncryptedSessionObject, SessionObjectKind};

    fn fixture(size: usize) -> (RelayManifest, Vec<u8>) {
        let capsule = [7; CAPSULE_BYTES];
        let mut bytes = vec![5; size];
        bytes[32..64].copy_from_slice(&hash(&capsule));
        let object = EncryptedSessionObject::new(
            SessionObjectKind::Message,
            "bot".into(),
            "conversation".into(),
            0,
            [0; 32],
            [1; 32],
            [2; 32],
            [3; 32],
            100,
            200,
            bytes.clone(),
            RELAY_CHUNK_BYTES,
        )
        .unwrap();
        (
            RelayManifest {
                header: object.header,
                signature: PeerSignature {
                    r: [4; 32],
                    s: [5; 32],
                },
                capsule: STANDARD.encode(capsule),
            },
            bytes,
        )
    }

    fn directory() -> std::path::PathBuf {
        let directory =
            std::env::temp_dir().join(format!("vibeface-session-relay-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&directory).unwrap();
        directory
    }

    fn upload(store: &mut SessionRelayStore, manifest: &RelayManifest, bytes: &[u8]) {
        store.begin(manifest, 100, u64::MAX, 10).unwrap();
        for (index, chunk) in bytes.chunks(RELAY_CHUNK_BYTES).enumerate() {
            store
                .put(
                    &manifest.header.object_id,
                    &manifest.header.sender_peer,
                    (index * RELAY_CHUNK_BYTES) as u64,
                    chunk,
                    100,
                )
                .unwrap();
        }
    }

    #[test]
    fn restart_retries_and_role_bound_complete_reads() -> Result<()> {
        let directory = directory();
        let (manifest, bytes) = fixture(RELAY_CHUNK_BYTES + 100);
        let id = &manifest.header.object_id;
        let sender = &manifest.header.sender_peer;
        let receiver = &manifest.header.receiver_peer;
        let first = &bytes[..RELAY_CHUNK_BYTES];
        let mut store = SessionRelayStore::open(&directory)?;
        assert_eq!(
            store
                .begin(&manifest, 100, bytes.len() as u64, 1)?
                .committed,
            0
        );
        assert!(store.manifest(id, receiver, 100).is_err());
        assert!(store.put(id, receiver, 0, first, 100).is_err());
        assert!(store.put(id, sender, 0, &first[..100], 100).is_err());
        assert!(store.put(id, sender, 8192, &bytes[8192..], 100).is_err());
        assert_eq!(store.put(id, sender, 0, first, 100)?.committed, 8192);
        assert_eq!(store.put(id, sender, 0, first, 100)?.committed, 8192);
        let mut changed = first.to_vec();
        changed[0] ^= 1;
        assert!(store.put(id, sender, 0, &changed, 100).is_err());
        assert!(store.get(id, receiver, 0, 100).is_err());
        drop(store);
        let mut store = SessionRelayStore::open(&directory)?;
        assert_eq!(
            store
                .begin(&manifest, 100, bytes.len() as u64, 1)?
                .committed,
            8192
        );
        let progress = store.put(id, sender, 8192, &bytes[8192..], 100)?;
        assert!(progress.complete);
        assert_eq!(store.put(id, sender, 0, first, 100)?, progress);
        assert_eq!(store.begin(&manifest, 100, 0, 0)?, progress);
        assert_eq!(store.manifest(id, receiver, 100)?, manifest);
        assert_eq!(store.get(id, receiver, 0, 100)?, first);
        assert_eq!(store.get(id, receiver, 8192, 100)?, bytes[8192..]);
        assert!(store.manifest(id, sender, 100).is_err());
        assert!(store.get(id, sender, 0, 100).is_err());
        assert!(store.get(id, receiver, 1, 100).is_err());
        assert!(store.get(id, receiver, bytes.len() as u64, 100).is_err());
        let mut changed = manifest.clone();
        changed.capsule = STANDARD.encode([9; CAPSULE_BYTES]);
        assert!(store.begin(&changed, 100, u64::MAX, 10).is_err());
        drop(store);
        std::fs::remove_dir_all(directory)?;
        Ok(())
    }

    #[test]
    fn downloads_resume_assemble_and_discard_in_a_separate_scoped_store() -> Result<()> {
        let directory = directory();
        let (manifest, bytes) = fixture(RELAY_CHUNK_BYTES + 100);
        let id = &manifest.header.object_id;
        let sender = &manifest.header.sender_peer;
        let receiver = &manifest.header.receiver_peer;
        let first = &bytes[..RELAY_CHUNK_BYTES];

        let mut relay = SessionRelayStore::open(&directory)?;
        upload(&mut relay, &manifest, &bytes);
        let mut downloads = SessionRelayStore::open_downloads(&directory)?;
        assert_eq!(
            downloads.begin(&manifest, 100, bytes.len() as u64, 1)?,
            RelayProgress {
                committed: 0,
                complete: false
            }
        );
        downloads.put(id, sender, 0, first, 100)?;
        drop(downloads);

        let mut downloads = SessionRelayStore::open_downloads(&directory)?;
        assert_eq!(
            downloads.begin(&manifest, 100, 0, 0)?,
            RelayProgress {
                committed: RELAY_CHUNK_BYTES as u64,
                complete: false
            }
        );
        assert!(downloads.complete_object(id, receiver, 100).is_err());
        downloads.put(
            id,
            sender,
            RELAY_CHUNK_BYTES as u64,
            &bytes[RELAY_CHUNK_BYTES..],
            100,
        )?;
        let object = downloads.complete_object(id, receiver, 100)?;
        assert_eq!(object.header, manifest.header);
        assert_eq!(object.signature, Some(manifest.signature));
        assert_eq!(object.ciphertext, bytes);
        assert!(downloads.complete_object(id, &[8; 32], 100).is_err());

        assert!(
            SessionRelayStore::open(&directory)?
                .complete_object(id, receiver, 100)
                .is_err()
        );
        assert!(directory.join("delivery/session-relay.sqlite3").exists());
        assert!(directory.join(SESSION_DOWNLOAD_DB).exists());
        downloads.discard_download(id, &[8; 32])?;
        assert!(downloads.complete_object(id, receiver, 100).is_ok());
        downloads.discard_download(id, receiver)?;
        downloads.discard_download(id, receiver)?;
        assert!(downloads.complete_object(id, receiver, 100).is_err());
        drop(relay);
        std::fs::remove_dir_all(directory)?;
        Ok(())
    }

    #[test]
    fn failed_final_hash_or_capsule_releases_reservation() -> Result<()> {
        let directory = directory();
        let (manifest, bytes) = fixture(RELAY_CHUNK_BYTES + 100);
        let id = &manifest.header.object_id;
        let sender = &manifest.header.sender_peer;
        let mut store = SessionRelayStore::open(&directory)?;
        store.begin(&manifest, 100, bytes.len() as u64, 1)?;
        store.put(id, sender, 0, &bytes[..8192], 100)?;
        let mut last = bytes[8192..].to_vec();
        last[0] ^= 1;
        assert!(store.put(id, sender, 8192, &last, 100).is_err());
        assert_eq!(
            store
                .db
                .query_row("SELECT count(*) FROM chunks", [], |row| row
                    .get::<_, u32>(0))?,
            0
        );
        assert_eq!(
            store
                .begin(&manifest, 100, bytes.len() as u64, 1)?
                .committed,
            0
        );
        store.put(id, sender, 0, &bytes[..8192], 100)?;
        store.put(id, sender, 8192, &bytes[8192..], 100)?;
        let (mut another, short) = fixture(100);
        another.capsule = STANDARD.encode([8; CAPSULE_BYTES]);
        store.begin(&another, 100, u64::MAX, 2)?;
        assert!(
            store
                .put(&another.header.object_id, sender, 0, &short, 100)
                .is_err()
        );
        assert_eq!(
            store
                .db
                .query_row("SELECT count(*) FROM objects", [], |row| row
                    .get::<_, u32>(0))?,
            1
        );
        assert_eq!(store.begin(&another, 100, u64::MAX, 2)?.committed, 0);
        drop(store);
        std::fs::remove_dir_all(directory)?;
        Ok(())
    }

    #[test]
    fn declared_capacity_order_limit_and_expiry_cleanup() -> Result<()> {
        let directory = directory();
        let (manifest, bytes) = fixture(RELAY_CHUNK_BYTES + 100);
        let (another, _) = fixture(100);
        let mut store = SessionRelayStore::open(&directory)?;
        assert!(store.begin(&manifest, 99, u64::MAX, 10).is_err());
        assert!(store.begin(&manifest, 100, u64::MAX, 0).is_err());
        assert!(
            store
                .begin(&manifest, 100, bytes.len() as u64 - 1, 10)
                .is_err()
        );
        store.begin(&manifest, 100, bytes.len() as u64, 10)?;
        assert!(store.begin(&another, 100, bytes.len() as u64, 10).is_err());
        assert!(store.begin(&another, 100, u64::MAX, 1).is_err());
        store.put(
            &manifest.header.object_id,
            &manifest.header.sender_peer,
            0,
            &bytes[..8192],
            100,
        )?;
        assert!(
            store
                .manifest(
                    &manifest.header.object_id,
                    &manifest.header.receiver_peer,
                    200
                )
                .is_err()
        );
        assert_eq!(
            store
                .db
                .query_row("SELECT count(*) FROM chunks", [], |row| row
                    .get::<_, u32>(0))?,
            0
        );
        assert!(store.begin(&manifest, 200, u64::MAX, 10).is_err());
        upload(&mut store, &another, &fixture(100).1);
        store.purge(200)?;
        assert_eq!(
            store
                .db
                .query_row("SELECT count(*) FROM objects", [], |row| row
                    .get::<_, u32>(0))?,
            0
        );
        drop(store);
        std::fs::remove_dir_all(directory)?;
        Ok(())
    }

    #[test]
    fn receiver_receipts_are_durable_and_visible_only_to_sender() -> Result<()> {
        let directory = directory();
        let (manifest, bytes) = fixture(100);
        let id = &manifest.header.object_id;
        let sender = &manifest.header.sender_peer;
        let receiver = &manifest.header.receiver_peer;
        let receipt: SessionStoredReceipt = serde_json::from_value(serde_json::json!({
            "object_id": id,
            "signature": "verified-by-caller"
        }))?;
        let mut store = SessionRelayStore::open(&directory)?;
        store.begin(&manifest, 100, 100, 1)?;
        assert!(store.put_receipt(id, receiver, &receipt, 100).is_err());
        upload(&mut store, &manifest, &bytes);
        assert!(store.receipt(id, sender, 100)?.is_none());
        assert!(store.put_receipt(id, sender, &receipt, 100).is_err());
        store.put_receipt(id, receiver, &receipt, 100)?;
        store.put_receipt(id, receiver, &receipt, 100)?;
        let changed =
            serde_json::from_value(serde_json::json!({ "object_id": id, "signature": "changed" }))?;
        assert!(store.put_receipt(id, receiver, &changed, 100).is_err());
        assert!(store.receipt(id, receiver, 100).is_err());
        drop(store);
        let mut store = SessionRelayStore::open(&directory)?;
        assert_eq!(store.receipt(id, sender, 100)?, Some(receipt));
        assert!(store.receipt(id, sender, 200).is_err());
        store.purge(200)?;
        drop(store);
        std::fs::remove_dir_all(directory)?;
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn sqlite_is_private_and_rejects_symlinks() -> Result<()> {
        use std::os::unix::fs::{PermissionsExt, symlink};
        let directory = directory();
        let store = SessionRelayStore::open(&directory)?;
        let path = directory.join("delivery/session-relay.sqlite3");
        assert_eq!(
            std::fs::metadata(&path)?.permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(
            std::fs::metadata(path.parent().unwrap())?
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        drop(store);
        let target = directory.join("target.sqlite3");
        std::fs::rename(&path, &target)?;
        symlink(&target, &path)?;
        assert!(SessionRelayStore::open(&directory).is_err());
        std::fs::remove_dir_all(directory)?;
        Ok(())
    }
}
