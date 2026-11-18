//! Durable sender cursor and one staged encrypted session batch per route.
//!
//! This store deliberately keeps the session payload opaque.  Endpoint delivery code remains
//! responsible for checking the object header, signature, ciphertext commitment, and capsule.

use super::session::SealedSessionObject;
use anyhow::{Context, Result, bail, ensure};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use std::path::Path;
use vibeface_delivery::{
    CAPSULE_BYTES, EncryptedSessionObject, MAX_SESSION_OBJECT_BYTES, PeerSignature,
};

const MAX_ROUTES: i64 = 64;
const MAX_OBJECTS: usize = 65;
const MAX_BATCH_BYTES: usize = 128 * 1024 * 1024;
const MAX_STAGED_BYTES: usize = 2 * MAX_BATCH_BYTES;
const MAX_ROUTE_BYTES: usize = 256;
const MAX_HEADER_BYTES: usize = 64 * 1024;

/// One sender batch, kept until the endpoint has accepted every object in it.
pub(super) struct StagedBatch {
    pub sequence: u64,
    pub objects: Vec<SealedSessionObject>,
}

pub(super) struct SessionOutboxStore {
    db: Connection,
}

#[derive(Clone)]
struct StoredObject {
    header: Vec<u8>,
    signature: Option<Vec<u8>>,
    ciphertext: Vec<u8>,
    capsule: Vec<u8>,
}

impl SessionOutboxStore {
    pub(super) fn open(data_dir: &Path) -> Result<Self> {
        let mut db = crate::store::private_file_connection(
            &data_dir.join("delivery/session-outbox.sqlite3"),
        )?;
        db.busy_timeout(std::time::Duration::from_secs(5))?;
        db.execute_batch(
            "PRAGMA foreign_keys=ON;
             PRAGMA journal_mode=DELETE;
             PRAGMA synchronous=FULL;
             PRAGMA secure_delete=ON;
             PRAGMA auto_vacuum=FULL;",
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
                    "unversioned session outbox database is unsupported"
                );
                tx.execute_batch(
                    "CREATE TABLE routes (
                        route TEXT PRIMARY KEY NOT NULL,
                        cursor INTEGER NOT NULL CHECK (cursor >= 0)
                    );
                    CREATE TABLE staged_batches (
                        route TEXT PRIMARY KEY NOT NULL REFERENCES routes(route) ON DELETE CASCADE,
                        sequence INTEGER NOT NULL CHECK (sequence >= 0),
                        object_count INTEGER NOT NULL CHECK (object_count BETWEEN 1 AND 65),
                        total_bytes INTEGER NOT NULL CHECK (total_bytes BETWEEN 1 AND 134217728)
                    );
                    CREATE TABLE staged_objects (
                        route TEXT NOT NULL REFERENCES staged_batches(route) ON DELETE CASCADE,
                        object_index INTEGER NOT NULL CHECK (object_index BETWEEN 0 AND 64),
                        header BLOB NOT NULL CHECK (length(header) BETWEEN 1 AND 65536),
                        signature BLOB CHECK (signature IS NULL OR length(signature) = 64),
                        ciphertext BLOB NOT NULL CHECK (length(ciphertext) BETWEEN 1 AND 16777216),
                        capsule BLOB NOT NULL CHECK (length(capsule) = 80),
                        PRIMARY KEY (route, object_index)
                    ) WITHOUT ROWID;
                    PRAGMA user_version = 1;",
                )?;
            }
            1 => {}
            _ => bail!(
                "unsupported session outbox database schema version {version}; initialize a fresh Node data directory"
            ),
        }
        tx.commit()?;
        Ok(Self { db })
    }

    pub(super) fn cursor(&self, route: &str) -> Result<Option<u64>> {
        validate_route(route)?;
        self.db
            .query_row("SELECT cursor FROM routes WHERE route=?1", [route], |row| {
                row.get::<_, i64>(0)
            })
            .optional()?
            .map(parse_sqlite_u64)
            .transpose()
    }

    pub(super) fn initialize(&mut self, route: &str, head: u64) -> Result<()> {
        validate_route(route)?;
        let head = sqlite_u64(head)?;
        let tx = self
            .db
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let current: Option<i64> = tx
            .query_row("SELECT cursor FROM routes WHERE route=?1", [route], |row| {
                row.get(0)
            })
            .optional()?;
        match current {
            None => {
                let count: i64 =
                    tx.query_row("SELECT count(*) FROM routes", [], |row| row.get(0))?;
                ensure!(count < MAX_ROUTES, "too many session outbox routes");
                tx.execute(
                    "INSERT INTO routes(route,cursor) VALUES(?1,?2)",
                    params![route, head],
                )?;
            }
            Some(current) if head <= current => {}
            Some(_) => {
                let staged: bool = tx.query_row(
                    "SELECT EXISTS(SELECT 1 FROM staged_batches WHERE route=?1)",
                    [route],
                    |row| row.get(0),
                )?;
                ensure!(
                    !staged,
                    "cannot initialize a session outbox route with a staged batch"
                );
                tx.execute(
                    "UPDATE routes SET cursor=?2 WHERE route=?1",
                    params![route, head],
                )?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    pub(super) fn batch(&self, route: &str) -> Result<Option<StagedBatch>> {
        validate_route(route)?;
        let row: Option<(i64, i64, i64)> = self
            .db
            .query_row(
                "SELECT sequence,object_count,total_bytes FROM staged_batches WHERE route=?1",
                [route],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()?;
        let Some((sequence, object_count, total_bytes)) = row else {
            return Ok(None);
        };
        let sequence = parse_sqlite_u64(sequence)?;
        let object_count = usize::try_from(object_count).context("invalid staged object count")?;
        let total_bytes = usize::try_from(total_bytes).context("invalid staged byte count")?;
        let objects = read_objects_as_session(&self.db, route, object_count, total_bytes)?;
        Ok(Some(StagedBatch { sequence, objects }))
    }

    pub(super) fn stage(
        &mut self,
        route: &str,
        expected_cursor: u64,
        sequence: u64,
        objects: &[SealedSessionObject],
    ) -> Result<()> {
        validate_route(route)?;
        ensure!(!objects.is_empty(), "session outbox batch is empty");
        ensure!(
            objects.len() <= MAX_OBJECTS,
            "session outbox batch has too many objects"
        );
        ensure!(
            sequence > expected_cursor,
            "session outbox sequence does not advance its cursor"
        );
        let expected_cursor = sqlite_u64(expected_cursor)?;
        let sequence = sqlite_u64(sequence)?;
        let stored = objects
            .iter()
            .map(encode_object)
            .collect::<Result<Vec<_>>>()?;
        let total_bytes = stored
            .iter()
            .try_fold(0usize, |total, object| {
                total.checked_add(object.ciphertext.len())
            })
            .context("session outbox batch size overflow")?;
        ensure!(
            total_bytes <= MAX_BATCH_BYTES,
            "session outbox batch exceeds the 128 MiB limit"
        );

        let tx = self
            .db
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let current: i64 = tx
            .query_row("SELECT cursor FROM routes WHERE route=?1", [route], |row| {
                row.get(0)
            })
            .context("session outbox route is not initialized")?;
        ensure!(
            current == expected_cursor,
            "session outbox cursor changed before staging"
        );

        let prior: Option<(i64, i64, i64)> = tx
            .query_row(
                "SELECT sequence,object_count,total_bytes FROM staged_batches WHERE route=?1",
                [route],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()?;
        if let Some((prior_sequence, prior_count, prior_bytes)) = prior {
            ensure!(
                prior_sequence == sequence,
                "another session outbox batch is already staged"
            );
            ensure!(
                prior_count == i64::try_from(stored.len()).context("object count overflow")?
                    && prior_bytes == i64::try_from(total_bytes).context("batch size overflow")?,
                "session outbox batch changed across retry"
            );
            let prior_objects = read_objects(&tx, route, stored.len(), total_bytes)?;
            ensure!(
                prior_objects
                    .iter()
                    .zip(stored.iter())
                    .all(|(prior, next)| stored_object_matches(prior, next)),
                "session outbox batch changed across retry"
            );
            tx.commit()?;
            return Ok(());
        }

        let occupied: u64 = tx.query_row(
            "SELECT coalesce(sum(total_bytes), 0) FROM staged_batches",
            [],
            |row| row.get(0),
        )?;
        ensure!(
            occupied.saturating_add(total_bytes as u64) <= MAX_STAGED_BYTES as u64,
            "session outbox disk quota is exhausted"
        );
        tx.execute(
            "INSERT INTO staged_batches(route,sequence,object_count,total_bytes)
             VALUES(?1,?2,?3,?4)",
            params![
                route,
                sequence,
                i64::try_from(stored.len()).context("object count overflow")?,
                i64::try_from(total_bytes).context("batch size overflow")?,
            ],
        )?;
        for (object_index, object) in stored.iter().enumerate() {
            tx.execute(
                "INSERT INTO staged_objects(route,object_index,header,signature,ciphertext,capsule)
                 VALUES(?1,?2,?3,?4,?5,?6)",
                params![
                    route,
                    i64::try_from(object_index).context("object index overflow")?,
                    &object.header,
                    object.signature.as_deref(),
                    &object.ciphertext,
                    &object.capsule,
                ],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    pub(super) fn advance(
        &mut self,
        route: &str,
        expected_cursor: u64,
        sequence: u64,
    ) -> Result<()> {
        validate_route(route)?;
        ensure!(
            sequence > expected_cursor,
            "session outbox sequence does not advance its cursor"
        );
        let expected_cursor = sqlite_u64(expected_cursor)?;
        let sequence = sqlite_u64(sequence)?;
        let tx = self
            .db
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let current: i64 = tx
            .query_row("SELECT cursor FROM routes WHERE route=?1", [route], |row| {
                row.get(0)
            })
            .context("session outbox route is not initialized")?;
        ensure!(
            current == expected_cursor,
            "session outbox cursor changed before advancing"
        );

        let staged: Option<(i64, i64, i64)> = tx
            .query_row(
                "SELECT sequence,object_count,total_bytes FROM staged_batches WHERE route=?1",
                [route],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()?;
        if let Some((staged_sequence, object_count, total_bytes)) = staged {
            ensure!(
                staged_sequence == sequence,
                "session outbox has a different staged sequence"
            );
            let object_count =
                usize::try_from(object_count).context("invalid staged object count")?;
            let total_bytes = usize::try_from(total_bytes).context("invalid staged byte count")?;
            // Refuse to discard a torn or manually modified batch.
            let _ = read_objects(&tx, route, object_count, total_bytes)?;
            tx.execute("DELETE FROM staged_batches WHERE route=?1", [route])?;
        }
        tx.execute(
            "UPDATE routes SET cursor=?2 WHERE route=?1 AND cursor=?3",
            params![route, sequence, expected_cursor],
        )?;
        ensure!(
            tx.changes() == 1,
            "session outbox cursor was changed concurrently"
        );
        tx.commit()?;
        Ok(())
    }
}

fn validate_route(route: &str) -> Result<()> {
    ensure!(!route.is_empty(), "session outbox route is empty");
    ensure!(
        route.len() <= MAX_ROUTE_BYTES,
        "session outbox route is too long"
    );
    ensure!(
        !route.as_bytes().contains(&0),
        "session outbox route contains a NUL byte"
    );
    Ok(())
}

fn sqlite_u64(value: u64) -> Result<i64> {
    i64::try_from(value).context("session outbox sequence exceeds SQLite integer range")
}

fn parse_sqlite_u64(value: i64) -> Result<u64> {
    ensure!(value >= 0, "session outbox contains a negative sequence");
    Ok(value as u64)
}

fn encode_object(object: &SealedSessionObject) -> Result<StoredObject> {
    let (object, capsule) = object;
    ensure!(
        !object.ciphertext.is_empty() && object.ciphertext.len() <= MAX_SESSION_OBJECT_BYTES,
        "session outbox object ciphertext size is invalid"
    );
    ensure!(
        capsule.len() == CAPSULE_BYTES,
        "session outbox object capsule size is invalid"
    );
    let header = serde_json::to_vec(&object.header)?;
    ensure!(
        !header.is_empty() && header.len() <= MAX_HEADER_BYTES,
        "session outbox object header is too large"
    );
    let signature = object
        .signature
        .map(PeerSignature::to_bytes)
        .map(|bytes| bytes.to_vec());
    Ok(StoredObject {
        header,
        signature,
        ciphertext: object.ciphertext.clone(),
        capsule: capsule.to_vec(),
    })
}

fn stored_object_matches(left: &StoredObject, right: &StoredObject) -> bool {
    left.header == right.header
        && left.signature == right.signature
        && left.ciphertext == right.ciphertext
        && left.capsule == right.capsule
}

fn read_objects(
    connection: &Connection,
    route: &str,
    expected_count: usize,
    expected_bytes: usize,
) -> Result<Vec<StoredObject>> {
    let mut statement = connection.prepare(
        "SELECT header,signature,ciphertext,capsule
         FROM staged_objects WHERE route=?1 ORDER BY object_index",
    )?;
    let rows = statement.query_map([route], |row| {
        Ok(StoredObject {
            header: row.get(0)?,
            signature: row.get(1)?,
            ciphertext: row.get(2)?,
            capsule: row.get(3)?,
        })
    })?;
    let mut stored = Vec::with_capacity(expected_count);
    for row in rows {
        stored.push(row?);
    }
    ensure!(
        stored.len() == expected_count,
        "session outbox staged object count is inconsistent"
    );
    let total_bytes = stored
        .iter()
        .try_fold(0usize, |total, object| {
            total.checked_add(object.ciphertext.len())
        })
        .context("session outbox staged byte count overflow")?;
    ensure!(
        total_bytes == expected_bytes && total_bytes <= MAX_BATCH_BYTES,
        "session outbox staged byte count is inconsistent"
    );
    Ok(stored)
}

fn decode_object(stored: StoredObject) -> Result<SealedSessionObject> {
    ensure!(
        stored.ciphertext.len() <= MAX_SESSION_OBJECT_BYTES,
        "session outbox object ciphertext is too large"
    );
    ensure!(
        stored.capsule.len() == CAPSULE_BYTES,
        "session outbox object capsule is invalid"
    );
    let header = serde_json::from_slice(&stored.header)?;
    let signature = match stored.signature {
        None => None,
        Some(bytes) => Some(PeerSignature::from_bytes(bytes.try_into().map_err(
            |_| anyhow::anyhow!("session outbox object signature is invalid"),
        )?)),
    };
    Ok((
        EncryptedSessionObject {
            header,
            ciphertext: stored.ciphertext,
            signature,
        },
        stored
            .capsule
            .try_into()
            .map_err(|_| anyhow::anyhow!("session outbox object capsule is invalid"))?,
    ))
}

fn read_objects_as_session(
    connection: &Connection,
    route: &str,
    expected_count: usize,
    expected_bytes: usize,
) -> Result<Vec<SealedSessionObject>> {
    read_objects(connection, route, expected_count, expected_bytes)?
        .into_iter()
        .map(decode_object)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use vibeface_delivery::{PeerSignature, SessionObjectKind};

    fn temp_dir() -> std::path::PathBuf {
        let path =
            std::env::temp_dir().join(format!("vibeface-session-outbox-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&path).expect("temporary directory");
        path
    }

    fn object(marker: u8) -> SealedSessionObject {
        let object = EncryptedSessionObject::new(
            SessionObjectKind::Message,
            "network".into(),
            "conversation".into(),
            0,
            [0; 32],
            [1; 32],
            [2; 32],
            [3; 32],
            1,
            2,
            vec![marker; 32],
            8,
        )
        .expect("object")
        .with_signature(PeerSignature::from_bytes([marker; 64]));
        (object, [marker; CAPSULE_BYTES])
    }

    #[test]
    fn restart_preserves_cursor_and_opaque_bytes() -> Result<()> {
        let dir = temp_dir();
        let route = "recipient/relay";
        let objects = vec![object(7), object(8)];
        {
            let mut store = SessionOutboxStore::open(&dir)?;
            assert_eq!(store.cursor(route)?, None);
            store.initialize(route, 10)?;
            store.stage(route, 10, 12, &objects)?;
            assert_eq!(store.cursor(route)?, Some(10));
            let staged = store.batch(route)?.expect("staged batch");
            assert_eq!(staged.sequence, 12);
            assert_eq!(staged.objects, objects);
        }
        {
            let mut store = SessionOutboxStore::open(&dir)?;
            assert_eq!(store.cursor(route)?, Some(10));
            assert_eq!(store.batch(route)?.expect("staged batch").objects, objects);
            store.advance(route, 10, 12)?;
            assert_eq!(store.cursor(route)?, Some(12));
            assert!(store.batch(route)?.is_none());
        }
        let _ = std::fs::remove_dir_all(dir);
        Ok(())
    }

    #[test]
    fn stage_is_idempotent_for_exact_bytes_and_rejects_partial_retry() -> Result<()> {
        let dir = temp_dir();
        let route = "route";
        let objects = vec![object(1)];
        let mut store = SessionOutboxStore::open(&dir)?;
        store.initialize(route, 0)?;
        store.stage(route, 0, 1, &objects)?;
        store.stage(route, 0, 1, &objects)?;
        let mut changed = objects.clone();
        changed[0].1[0] ^= 1;
        assert!(store.stage(route, 0, 1, &changed).is_err());
        assert_eq!(store.batch(route)?.expect("staged batch").objects, objects);
        assert!(store.stage(route, 0, 2, &objects).is_err());
        let _ = std::fs::remove_dir_all(dir);
        Ok(())
    }

    #[test]
    fn advance_without_a_batch_skips_and_never_rewinds() -> Result<()> {
        let dir = temp_dir();
        let mut store = SessionOutboxStore::open(&dir)?;
        store.initialize("a", 3)?;
        store.initialize("a", 2)?;
        assert_eq!(store.cursor("a")?, Some(3));
        store.advance("a", 3, 9)?;
        assert_eq!(store.cursor("a")?, Some(9));
        assert!(store.advance("a", 9, 8).is_err());
        let _ = std::fs::remove_dir_all(dir);
        Ok(())
    }
}
