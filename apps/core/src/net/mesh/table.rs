//! The mesh records this Node holds, kept in `mesh.sqlite3`: a public cache it can always rebuild
//! from its bootstrap nodes, apart from `node.sqlite3` and its schema guard. The relay tables in
//! the same file belong to their callers.
use super::record::{MeshRecord, SignedMeshRecord};
use anyhow::{Result, ensure};
use rusqlite::{Connection, OpenFlags, params};
use std::{collections::HashMap, path::Path};

pub const CAPACITY: usize = 65536;
/// A device's next record is dated at least this long after the one held. Its own date, not
/// when a copy arrives: a copy pulled late must not hold back the next one broadcast.
pub const MIN_INTERVAL_MS: u64 = 30_000;
/// Records seen this recently stay, expired or not, as candidates for joining again.
const KEEP_MS: u64 = 30 * 24 * 3600 * 1000;

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS mesh_records(device_id TEXT PRIMARY KEY, seq INTEGER NOT NULL,
  expires_at INTEGER NOT NULL, last_seen_at INTEGER NOT NULL, reachable TEXT NOT NULL,
  record TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS mesh_relays(url TEXT PRIMARY KEY, device_id TEXT NOT NULL,
  score REAL NOT NULL, latency_ms INTEGER, last_ok_at INTEGER, last_fail_at INTEGER,
  fails INTEGER NOT NULL);
CREATE TABLE IF NOT EXISTS relay_usage(endpoint_id TEXT PRIMARY KEY, bytes_in INTEGER NOT NULL,
  bytes_out INTEGER NOT NULL, connections INTEGER NOT NULL, first_seen_at INTEGER NOT NULL,
  last_seen_at INTEGER NOT NULL);";

#[derive(Clone, Debug)]
pub struct Entry {
    pub signed: SignedMeshRecord,
    pub record: MeshRecord,
    /// When this Node accepted the record.
    pub last_seen_ms: u64,
}

impl Entry {
    pub fn online(&self, now_ms: u64) -> bool {
        now_ms < self.record.expires_at()
    }
}

pub struct Table {
    /// `mesh.sqlite3`, also holding `mesh_relays` and `relay_usage`.
    pub db: Connection,
    entries: HashMap<String, Entry>,
    own: String,
    network: String,
    capacity: usize,
}

impl Table {
    /// Loads the records of `network` seen in the last 30 days; `own` is this Node's device ID.
    pub fn open(path: &Path, own: &str, network: &str, now_ms: u64) -> Result<Self> {
        let db = crate::store::private_file_connection(path)?;
        db.busy_timeout(std::time::Duration::from_secs(5))?;
        db.execute_batch(&format!("PRAGMA journal_mode=WAL;{SCHEMA}"))?;
        db.execute(
            "DELETE FROM mesh_records WHERE last_seen_at < ?1",
            [now_ms.saturating_sub(KEEP_MS) as i64],
        )?;
        let (entries, stale) = load(&db, network)?;
        for device in stale {
            db.execute("DELETE FROM mesh_records WHERE device_id = ?1", [device])?;
        }
        Ok(Self {
            db,
            entries,
            own: own.to_owned(),
            network: network.to_owned(),
            capacity: CAPACITY,
        })
    }

    /// Takes `records` from other devices; returns those accepted, each valid and dated at least
    /// 30 s after the one held.
    pub fn insert(
        &mut self,
        records: Vec<SignedMeshRecord>,
        now_ms: u64,
    ) -> Result<Vec<MeshRecord>> {
        let mut accepted = Vec::new();
        let transaction = self.db.unchecked_transaction()?;
        for signed in records {
            let Ok(record) = signed.open(&self.network, now_ms) else {
                continue;
            };
            if record.device_id == self.own {
                continue;
            }
            if let Some(entry) = self.entries.get(&record.device_id)
                && record.seq < entry.record.seq.saturating_add(MIN_INTERVAL_MS)
            {
                continue;
            }
            put(
                &transaction,
                &mut self.entries,
                &self.own,
                self.capacity,
                signed,
                record.clone(),
                now_ms,
            )?;
            accepted.push(record);
        }
        transaction.commit()?;
        Ok(accepted)
    }

    /// Stores this Node's own record.
    pub fn insert_own(
        &mut self,
        signed: SignedMeshRecord,
        record: MeshRecord,
        now_ms: u64,
    ) -> Result<()> {
        let transaction = self.db.unchecked_transaction()?;
        put(
            &transaction,
            &mut self.entries,
            &self.own,
            self.capacity,
            signed,
            record,
            now_ms,
        )?;
        transaction.commit()?;
        Ok(())
    }

    pub fn get(&self, device_id: &str) -> Option<&Entry> {
        self.entries.get(device_id)
    }

    /// Every record held, expired ones too.
    pub fn entries(&self) -> impl Iterator<Item = &Entry> {
        self.entries.values()
    }

    pub fn online(&self, now_ms: u64) -> impl Iterator<Item = &Entry> {
        self.entries
            .values()
            .filter(move |entry| entry.online(now_ms))
    }
}

/// The records of `network` in `path`, read as another process reads them while the Node runs,
/// without changing the file.
pub fn read(path: &Path, network: &str) -> Result<Vec<Entry>> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) => ensure!(
            vibeface_bridge::platform::privatefs::is_plain_file(&metadata),
            "database must be a regular file"
        ),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(vec![]),
        Err(error) => return Err(error.into()),
    }
    let db = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NOFOLLOW,
    )?;
    db.busy_timeout(std::time::Duration::from_secs(5))?;
    Ok(load(&db, network)?.0.into_values().collect())
}

/// The rows of `network`, and the devices whose rows belong elsewhere or no longer parse.
fn load(db: &Connection, network: &str) -> Result<(HashMap<String, Entry>, Vec<String>)> {
    let mut entries = HashMap::new();
    let mut stale = Vec::new();
    let mut statement = db.prepare("SELECT device_id, last_seen_at, record FROM mesh_records")?;
    let mut rows = statement.query([])?;
    while let Some(row) = rows.next()? {
        let device: String = row.get(0)?;
        let last_seen_ms = row.get::<_, i64>(1)? as u64;
        let loaded = serde_json::from_str::<SignedMeshRecord>(&row.get::<_, String>(2)?)
            .ok()
            .and_then(|signed| {
                let record = MeshRecord::parse(&signed.payload).ok()?;
                (record.device_id == device && record.network == network)
                    .then_some((signed, record))
            });
        match loaded {
            Some((signed, record)) => {
                entries.insert(
                    device,
                    Entry {
                        signed,
                        record,
                        last_seen_ms,
                    },
                );
            }
            None => stale.push(device),
        }
    }
    Ok((entries, stale))
}

fn put(
    db: &Connection,
    entries: &mut HashMap<String, Entry>,
    own: &str,
    capacity: usize,
    signed: SignedMeshRecord,
    record: MeshRecord,
    now_ms: u64,
) -> Result<()> {
    if !entries.contains_key(&record.device_id) && entries.len() >= capacity {
        // ponytail: a scan of the full table per insert once full; an ordered index if it shows.
        let victim = entries
            .values()
            .filter(|entry| entry.record.device_id != own)
            .min_by_key(|entry| (entry.online(now_ms), entry.last_seen_ms))
            .map(|entry| entry.record.device_id.clone());
        if let Some(victim) = victim {
            entries.remove(&victim);
            db.execute("DELETE FROM mesh_records WHERE device_id = ?1", [&victim])?;
        }
    }
    db.execute(
        "INSERT OR REPLACE INTO mesh_records(device_id, seq, expires_at, last_seen_at, reachable, record)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![
            record.device_id,
            record.seq as i64,
            record.expires_at() as i64,
            now_ms as i64,
            serde_json::to_value(record.reachable)?.as_str().unwrap_or_default(),
            serde_json::to_string(&signed)?,
        ],
    )?;
    entries.insert(
        record.device_id.clone(),
        Entry {
            signed,
            record,
            last_seen_ms: now_ms,
        },
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::super::record::{DEFAULT_TTL_S, DOMAIN, Reachable};
    use super::*;

    const NOW: u64 = 1_800_000_000_000;

    fn dir(label: &str) -> std::path::PathBuf {
        let dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../.artifacts/mesh-tests")
            .join(format!("{label}-{}", uuid::Uuid::new_v4()));
        crate::config::private_dir(&dir).unwrap();
        dir
    }

    fn signed(identity: &vibeface_secure::Identity, seq: u64) -> SignedMeshRecord {
        let record = MeshRecord {
            v: 1,
            network: "lab".into(),
            device_id: identity.device_id(),
            seq,
            ttl_s: DEFAULT_TTL_S,
            addrs: vec![],
            relay_url: None,
            reachable: Reachable::Unknown,
            roles: vec![],
        };
        SignedMeshRecord::sign(&record, |payload| Ok(identity.sign(DOMAIN, payload)?)).unwrap()
    }

    fn seq(table: &Table, identity: &vibeface_secure::Identity) -> Option<u64> {
        table
            .get(&identity.device_id())
            .map(|entry| entry.record.seq)
    }

    #[test]
    fn a_device_moves_forward_at_most_every_thirty_seconds() -> Result<()> {
        let own = vibeface_secure::Identity::generate()?;
        let peer = vibeface_secure::Identity::generate()?;
        let dir = dir("forward");
        let mut table = Table::open(&dir.join("mesh.sqlite3"), &own.device_id(), "lab", NOW)?;
        assert_eq!(table.insert(vec![signed(&peer, NOW)], NOW)?.len(), 1);
        assert!(
            table
                .insert(vec![signed(&peer, NOW - 1)], NOW + 60_000)?
                .is_empty()
        );
        assert!(
            table
                .insert(vec![signed(&peer, NOW + 10_000)], NOW + 40_000)?
                .is_empty()
        );
        assert_eq!(seq(&table, &peer), Some(NOW));
        // The first copy came late; the next is dated 30 s on and counts however soon it comes.
        assert_eq!(
            table
                .insert(vec![signed(&peer, NOW + 30_000)], NOW + 1_000)?
                .len(),
            1
        );
        assert_eq!(seq(&table, &peer), Some(NOW + 30_000));
        assert!(
            table
                .insert(vec![signed(&own, NOW + 40_000)], NOW + 40_000)?
                .is_empty()
        );
        Ok(())
    }

    #[test]
    fn a_full_table_drops_expired_records_then_the_longest_unseen() -> Result<()> {
        let own = vibeface_secure::Identity::generate()?;
        let [a, b, c, d] = std::array::from_fn(|_| vibeface_secure::Identity::generate().unwrap());
        let dir = dir("full");
        let mut table = Table::open(&dir.join("mesh.sqlite3"), &own.device_id(), "lab", NOW)?;
        table.capacity = 2;
        let ttl = u64::from(DEFAULT_TTL_S) * 1000;
        table.insert(vec![signed(&a, NOW)], NOW)?;
        table.insert(vec![signed(&b, NOW + ttl / 2)], NOW + ttl / 2)?;
        // a has expired: it goes before b, though b was seen later.
        let later = NOW + ttl + 1;
        table.insert(vec![signed(&c, later)], later)?;
        assert_eq!((seq(&table, &a), seq(&table, &c).is_some()), (None, true));
        table.insert(vec![signed(&d, later + 1)], later + 1)?;
        assert_eq!((seq(&table, &b), seq(&table, &d).is_some()), (None, true));
        Ok(())
    }

    #[test]
    fn records_outlive_a_restart_until_thirty_days_unseen() -> Result<()> {
        let own = vibeface_secure::Identity::generate()?;
        let [fresh, old] = std::array::from_fn(|_| vibeface_secure::Identity::generate().unwrap());
        let dir = dir("restart");
        let path = dir.join("mesh.sqlite3");
        let mut table = Table::open(&path, &own.device_id(), "lab", NOW)?;
        table.insert(vec![signed(&old, NOW)], NOW)?;
        let later = NOW + 20 * 24 * 3600 * 1000;
        table.insert(vec![signed(&fresh, later)], later)?;
        drop(table);
        let restarted = Table::open(
            &path,
            &own.device_id(),
            "lab",
            later + 15 * 24 * 3600 * 1000,
        )?;
        assert_eq!(seq(&restarted, &fresh), Some(later));
        assert_eq!(seq(&restarted, &old), None);
        assert_eq!(restarted.online(later + 1).count(), 1);
        drop(restarted);
        let other = Table::open(&path, &own.device_id(), "other", later)?;
        assert_eq!(other.entries().count(), 0);
        Ok(())
    }
}
