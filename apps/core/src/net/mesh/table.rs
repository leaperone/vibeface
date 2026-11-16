//! The mesh records this Node holds, kept in `mesh.sqlite3`: a public cache it can always rebuild
//! from its bootstrap nodes, apart from `node.sqlite3` and its schema guard. The table lives in
//! memory; each change comes back as [`Write`]s the caller hands to [`Store`] later, in a batch
//! and without holding the table. The relay tables in the same file belong to their callers.
use super::record::{MeshRecord, SignedMeshRecord};
use anyhow::{Context as _, Result, ensure};
use rusqlite::{Connection, OpenFlags, params};
use std::{
    collections::{HashMap, HashSet},
    path::Path,
};

pub const CAPACITY: usize = 65536;
/// A device's next record is dated at least this long after the one held. Its own date, not
/// when a copy arrives: a copy pulled late must not hold back the next one broadcast.
pub const MIN_INTERVAL_MS: u64 = 30_000;
/// Records seen this recently stay, expired or not, as candidates for joining again; relay
/// checks and usage are kept as long.
pub const KEEP_MS: u64 = 30 * 24 * 3600 * 1000;

pub(crate) const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS mesh_records(device_id TEXT PRIMARY KEY, seq INTEGER NOT NULL,
  expires_at INTEGER NOT NULL, first_seen_at INTEGER NOT NULL, last_seen_at INTEGER NOT NULL,
  admission_order INTEGER NOT NULL UNIQUE CHECK(admission_order > 0),
  reachable TEXT NOT NULL, record TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS mesh_relays(url TEXT PRIMARY KEY, device_id TEXT NOT NULL,
  history INTEGER NOT NULL, latency_ms INTEGER, ip TEXT, last_ok_at INTEGER,
  last_fail_at INTEGER, fails INTEGER NOT NULL);
CREATE TABLE IF NOT EXISTS relay_usage(endpoint_id TEXT PRIMARY KEY, bytes_in INTEGER NOT NULL,
  bytes_out INTEGER NOT NULL, connections INTEGER NOT NULL, first_seen_at INTEGER NOT NULL,
  last_seen_at INTEGER NOT NULL);";

#[derive(Clone, Debug)]
pub struct Entry {
    pub signed: SignedMeshRecord,
    pub record: MeshRecord,
    /// When this Node first accepted a record of the device.
    pub first_seen_ms: u64,
    /// When this Node accepted the record.
    pub last_seen_ms: u64,
}

impl Entry {
    pub fn online(&self, now_ms: u64) -> bool {
        now_ms < self.record.expires_at()
    }
}

struct StoredEntry {
    entry: Entry,
    /// Local admission order, independent of record and reception timestamps.
    admission_order: i64,
}

/// A change of the table still to be written to `mesh.sqlite3`.
#[derive(Debug)]
pub enum Write {
    Put { entry: Entry, admission_order: i64 },
    Delete(String),
}

/// `mesh.sqlite3`'s connection, which writes the table's changes.
pub struct Store(Connection);

impl Store {
    /// Writes `writes` in one transaction, in order.
    pub fn apply(&mut self, writes: Vec<Write>) -> Result<()> {
        if writes.is_empty() {
            return Ok(());
        }
        let transaction = self.0.transaction()?;
        for write in writes {
            match write {
                Write::Put {
                    entry,
                    admission_order,
                } => {
                    transaction.execute(
                        "INSERT INTO mesh_records(device_id, seq, expires_at,
                           first_seen_at, last_seen_at, reachable, record, admission_order)
                         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
                         ON CONFLICT(device_id) DO UPDATE SET seq = excluded.seq,
                           expires_at = excluded.expires_at,
                           first_seen_at = excluded.first_seen_at,
                           last_seen_at = excluded.last_seen_at,
                           reachable = excluded.reachable, record = excluded.record,
                           admission_order = excluded.admission_order",
                        params![
                            entry.record.device_id,
                            entry.record.seq as i64,
                            entry.record.expires_at() as i64,
                            entry.first_seen_ms as i64,
                            entry.last_seen_ms as i64,
                            serde_json::to_value(entry.record.reachable)?
                                .as_str()
                                .unwrap_or_default(),
                            serde_json::to_string(&entry.signed)?,
                            admission_order,
                        ],
                    )?;
                }
                Write::Delete(device) => {
                    transaction
                        .execute("DELETE FROM mesh_records WHERE device_id = ?1", [device])?;
                }
            }
        }
        transaction.commit()?;
        Ok(())
    }
}

pub struct Table {
    entries: HashMap<String, StoredEntry>,
    last_admission_order: i64,
    own: String,
    network: String,
    capacity: usize,
    /// Devices whose records a full table never drops: the personal network's Nodes and the
    /// bootstrap nodes.
    protected: HashSet<String>,
}

impl Table {
    /// Loads the records of `network` seen in the last 30 days; `own` is this Node's device ID.
    pub fn open(path: &Path, own: &str, network: &str, now_ms: u64) -> Result<(Self, Store)> {
        let mut db = crate::store::private_file_connection(path)?;
        db.busy_timeout(std::time::Duration::from_secs(5))?;
        let transaction = db.transaction()?;
        transaction.execute_batch(SCHEMA)?;
        let (mut entries, stale) = load(&transaction, network)
            .context("mesh cache records do not have the required format")?;
        let last_admission_order = entries
            .values()
            .map(|entry| entry.admission_order)
            .max()
            .unwrap_or(0);
        let before = now_ms.saturating_sub(KEEP_MS);
        transaction.execute(
            "DELETE FROM mesh_records WHERE last_seen_at < ?1",
            [before as i64],
        )?;
        entries.retain(|_, entry| entry.entry.last_seen_ms >= before);
        for device in stale {
            transaction.execute("DELETE FROM mesh_records WHERE device_id = ?1", [device])?;
        }
        transaction.commit()?;
        db.execute_batch("PRAGMA journal_mode=WAL;")?;
        let table = Self {
            entries,
            last_admission_order,
            own: own.to_owned(),
            network: network.to_owned(),
            capacity: CAPACITY,
            protected: HashSet::new(),
        };
        Ok((table, Store(db)))
    }

    pub fn set_protected(&mut self, devices: HashSet<String>) {
        self.protected = devices;
    }

    pub fn is_protected(&self, device_id: &str) -> bool {
        self.protected.contains(device_id)
    }

    #[cfg(test)]
    pub fn set_capacity(&mut self, capacity: usize) {
        self.capacity = capacity;
    }

    /// Takes `records` from other devices; returns those accepted, each valid and dated at least
    /// 30 s after the one held, and the writes that store them. A device not held yet is taken
    /// only when it is protected or `admit` agrees.
    pub fn insert(
        &mut self,
        records: Vec<SignedMeshRecord>,
        now_ms: u64,
        mut admit: impl FnMut(&MeshRecord) -> bool,
    ) -> (Vec<MeshRecord>, Vec<Write>) {
        let mut accepted = Vec::new();
        let mut writes = Vec::new();
        for signed in records {
            let Ok(record) = signed.open(&self.network, now_ms) else {
                continue;
            };
            if record.device_id == self.own {
                continue;
            }
            match self.entries.get(&record.device_id) {
                Some(entry)
                    if record.seq < entry.entry.record.seq.saturating_add(MIN_INTERVAL_MS) =>
                {
                    continue;
                }
                None if !self.protected.contains(&record.device_id) && !admit(&record) => {
                    continue;
                }
                _ => {}
            }
            if self.put(signed, record.clone(), now_ms, &mut writes) {
                accepted.push(record);
            }
        }
        (accepted, writes)
    }

    /// Stores this Node's own record.
    pub fn insert_own(
        &mut self,
        signed: SignedMeshRecord,
        record: MeshRecord,
        now_ms: u64,
    ) -> Vec<Write> {
        let mut writes = Vec::new();
        self.put(signed, record, now_ms, &mut writes);
        writes
    }

    pub fn get(&self, device_id: &str) -> Option<&Entry> {
        self.entries.get(device_id).map(|entry| &entry.entry)
    }

    /// Every record held, expired ones too.
    pub fn entries(&self) -> impl Iterator<Item = &Entry> {
        self.entries.values().map(|entry| &entry.entry)
    }

    pub fn online(&self, now_ms: u64) -> impl Iterator<Item = &Entry> {
        self.entries().filter(move |entry| entry.online(now_ms))
    }

    /// The records a full table drops for new devices, one 64th of it in one scan: the expired
    /// ones seen longest ago first, then the ones admitted most recently, so records known for
    /// long outlast a flood of new ones. This Node's own record and protected ones are never
    /// dropped.
    fn victims(&self, now_ms: u64) -> Vec<String> {
        let mut droppable: Vec<((bool, u64), &str)> = self
            .entries
            .values()
            .filter(|stored| {
                let entry = &stored.entry;
                entry.record.device_id != self.own
                    && !self.protected.contains(&entry.record.device_id)
            })
            .map(|stored| {
                let entry = &stored.entry;
                let key = if entry.online(now_ms) {
                    (true, u64::MAX - stored.admission_order as u64)
                } else {
                    (false, entry.last_seen_ms)
                };
                (key, entry.record.device_id.as_str())
            })
            .collect();
        let count = (self.capacity / 64).clamp(1, droppable.len().max(1));
        if droppable.len() > count {
            droppable.select_nth_unstable(count);
            droppable.truncate(count);
        }
        droppable
            .into_iter()
            .map(|(_, device)| device.to_owned())
            .collect()
    }

    /// Holds `record`, dropping others when the table is full; returns false when the table is
    /// full of records that stay and `record` is neither this Node's nor protected, or a new
    /// device would exhaust the persisted admission order.
    fn put(
        &mut self,
        signed: SignedMeshRecord,
        record: MeshRecord,
        now_ms: u64,
        writes: &mut Vec<Write>,
    ) -> bool {
        let held = self.entries.get(&record.device_id);
        let first_seen_ms = held.map_or(now_ms, |entry| entry.entry.first_seen_ms);
        let admission_order = match held {
            Some(entry) => entry.admission_order,
            None => match self.last_admission_order.checked_add(1) {
                Some(order) => order,
                None => return false,
            },
        };
        if held.is_none() && self.entries.len() >= self.capacity {
            let victims = self.victims(now_ms);
            if victims.is_empty()
                && record.device_id != self.own
                && !self.protected.contains(&record.device_id)
            {
                return false;
            }
            for victim in victims {
                self.entries.remove(&victim);
                writes.push(Write::Delete(victim));
            }
        }
        let entry = Entry {
            signed,
            record,
            first_seen_ms,
            last_seen_ms: now_ms,
        };
        self.entries.insert(
            entry.record.device_id.clone(),
            StoredEntry {
                entry: entry.clone(),
                admission_order,
            },
        );
        self.last_admission_order = self.last_admission_order.max(admission_order);
        writes.push(Write::Put {
            entry,
            admission_order,
        });
        true
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
    Ok(load(&db, network)?
        .0
        .into_values()
        .map(|entry| entry.entry)
        .collect())
}

/// The rows of `network`, and the devices whose rows belong elsewhere or no longer parse.
fn load(db: &Connection, network: &str) -> Result<(HashMap<String, StoredEntry>, Vec<String>)> {
    let mut entries = HashMap::new();
    let mut stale = Vec::new();
    let mut admission_orders = HashSet::new();
    let mut statement = db.prepare(
        "SELECT device_id, first_seen_at, last_seen_at, record, admission_order FROM mesh_records",
    )?;
    let mut rows = statement.query([])?;
    while let Some(row) = rows.next()? {
        let device: String = row.get(0)?;
        let first_seen_ms = row.get::<_, i64>(1)? as u64;
        let last_seen_ms = row.get::<_, i64>(2)? as u64;
        let admission_order: i64 = row.get(4)?;
        ensure!(
            admission_order > 0 && admission_orders.insert(admission_order),
            "mesh admission orders must be positive and unique"
        );
        let loaded = serde_json::from_str::<SignedMeshRecord>(&row.get::<_, String>(3)?)
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
                    StoredEntry {
                        entry: Entry {
                            signed,
                            record,
                            first_seen_ms,
                            last_seen_ms,
                        },
                        admission_order,
                    },
                );
            }
            None => stale.push(device),
        }
    }
    Ok((entries, stale))
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

    fn insert(table: &mut Table, records: Vec<SignedMeshRecord>, now: u64) -> usize {
        table.insert(records, now, |_| true).0.len()
    }

    #[test]
    fn a_clock_backstep_does_not_replace_known_records_with_newcomers() -> Result<()> {
        let own = vibeface_secure::Identity::generate()?;
        let [a, b, c, d] = std::array::from_fn(|_| vibeface_secure::Identity::generate().unwrap());
        let dir = dir("admission-backstep");
        let (mut table, _) = Table::open(&dir.join("mesh.sqlite3"), &own.device_id(), "lab", NOW)?;
        table.set_capacity(3);
        assert_eq!(
            insert(&mut table, vec![signed(&a, NOW), signed(&b, NOW)], NOW),
            2
        );
        assert_eq!(insert(&mut table, vec![signed(&c, NOW)], NOW - 1_000), 1);
        assert_eq!(insert(&mut table, vec![signed(&d, NOW)], NOW - 999), 1);
        assert_eq!(
            [
                seq(&table, &a),
                seq(&table, &b),
                seq(&table, &c),
                seq(&table, &d)
            ],
            [Some(NOW), Some(NOW), None, Some(NOW)]
        );
        Ok(())
    }

    #[test]
    fn equal_reception_times_keep_admission_order() -> Result<()> {
        let own = vibeface_secure::Identity::generate()?;
        let mut peers: [_; 4] =
            std::array::from_fn(|_| vibeface_secure::Identity::generate().unwrap());
        peers.sort_by_key(|peer| peer.device_id());
        let [a, b, c, d] = peers;
        let dir = dir("equal-admission-times");
        let (mut table, _) = Table::open(&dir.join("mesh.sqlite3"), &own.device_id(), "lab", NOW)?;
        table.set_capacity(3);
        for peer in [&a, &b, &c, &d] {
            assert_eq!(insert(&mut table, vec![signed(peer, NOW)], NOW), 1);
        }
        assert_eq!(
            [
                seq(&table, &a),
                seq(&table, &b),
                seq(&table, &c),
                seq(&table, &d)
            ],
            [Some(NOW), Some(NOW), None, Some(NOW)]
        );
        Ok(())
    }

    #[test]
    fn queued_updates_and_readmissions_preserve_order_across_a_restart() -> Result<()> {
        let own = vibeface_secure::Identity::generate()?;
        let [a, b, c, d, e] =
            std::array::from_fn(|_| vibeface_secure::Identity::generate().unwrap());
        let dir = dir("persisted-admission-order");
        let path = dir.join("mesh.sqlite3");
        let (mut table, mut store) = Table::open(&path, &own.device_id(), "lab", NOW)?;
        table.set_capacity(3);
        let (accepted, mut pending) =
            table.insert(vec![signed(&a, NOW), signed(&b, NOW)], NOW, |_| true);
        assert_eq!(accepted.len(), 2);
        for (peer, seq, received) in [
            (&c, NOW, NOW - 1_000),
            (&a, NOW + 30_000, NOW - 900),
            (&d, NOW, NOW - 999),
            (&c, NOW, NOW - 998),
        ] {
            let (accepted, writes) = table.insert(vec![signed(peer, seq)], received, |_| true);
            assert_eq!(accepted.len(), 1);
            pending.extend(writes);
        }
        assert_eq!(
            [
                seq(&table, &a),
                seq(&table, &b),
                seq(&table, &c),
                seq(&table, &d)
            ],
            [Some(NOW + 30_000), Some(NOW), Some(NOW), None]
        );
        let later = pending.split_off(3);
        store.apply(pending)?;
        store.apply(later)?;
        drop((table, store));

        let (mut table, mut store) = Table::open(&path, &own.device_id(), "lab", NOW - 997)?;
        table.set_capacity(3);
        assert_eq!(seq(&table, &a), Some(NOW + 30_000));
        let held = table.get(&a.device_id()).unwrap();
        assert_eq!((held.first_seen_ms, held.last_seen_ms), (NOW, NOW - 900));
        let (accepted, writes) = table.insert(vec![signed(&e, NOW)], NOW - 2_000, |_| true);
        assert_eq!(accepted.len(), 1);
        store.apply(writes)?;
        assert_eq!(
            [
                seq(&table, &a),
                seq(&table, &b),
                seq(&table, &c),
                seq(&table, &e)
            ],
            [Some(NOW + 30_000), Some(NOW), None, Some(NOW)]
        );
        assert_eq!(
            store.0.query_row(
                "SELECT admission_order, first_seen_at, last_seen_at FROM mesh_records
                 WHERE device_id = ?1",
                [e.device_id()],
                |row| Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, u64>(1)?,
                    row.get::<_, u64>(2)?
                ))
            )?,
            (6, NOW - 2_000, NOW - 2_000)
        );
        assert_eq!(
            store.0.query_row(
                "SELECT admission_order FROM mesh_records WHERE device_id = ?1",
                [a.device_id()],
                |row| row.get::<_, i64>(0)
            )?,
            1
        );
        assert_eq!(read(&path, "lab")?.len(), 3);
        Ok(())
    }

    #[test]
    fn own_and_protected_records_survive_newer_admissions_and_expiry() -> Result<()> {
        let [own, member, a, b, c] =
            std::array::from_fn(|_| vibeface_secure::Identity::generate().unwrap());
        let dir = dir("protected-admission-order");
        let (mut table, _) = Table::open(&dir.join("mesh.sqlite3"), &own.device_id(), "lab", NOW)?;
        table.set_capacity(3);
        table.set_protected(HashSet::from([member.device_id()]));
        let signed_own = signed(&own, NOW);
        let own_record = signed_own.open("lab", NOW)?;
        assert_eq!(table.insert_own(signed_own, own_record, NOW).len(), 1);
        for (peer, received) in [(&member, NOW - 1), (&a, NOW - 2), (&b, NOW - 3)] {
            assert_eq!(insert(&mut table, vec![signed(peer, NOW)], received), 1);
        }
        let later = NOW + u64::from(DEFAULT_TTL_S) * 1_000 + 1;
        assert_eq!(insert(&mut table, vec![signed(&c, later)], later), 1);
        assert_eq!(
            [
                seq(&table, &own),
                seq(&table, &member),
                seq(&table, &a),
                seq(&table, &b),
                seq(&table, &c)
            ],
            [Some(NOW), Some(NOW), None, None, Some(later)]
        );
        assert_eq!(table.entries().count(), 3);
        Ok(())
    }

    #[test]
    fn exhausted_admission_order_refuses_new_devices_before_eviction() -> Result<()> {
        let [own, a, b, c] =
            std::array::from_fn(|_| vibeface_secure::Identity::generate().unwrap());
        let dir = dir("admission-order-overflow");
        let path = dir.join("mesh.sqlite3");
        let (mut table, mut store) = Table::open(&path, &own.device_id(), "lab", NOW)?;
        store.apply(
            table
                .insert(vec![signed(&a, NOW), signed(&b, NOW)], NOW, |_| true)
                .1,
        )?;
        store.0.execute(
            "UPDATE mesh_records SET admission_order = ?1 WHERE device_id = ?2",
            params![i64::MAX, b.device_id()],
        )?;
        drop((table, store));

        let (mut table, mut store) = Table::open(&path, &own.device_id(), "lab", NOW)?;
        table.set_capacity(2);
        let (accepted, writes) = table.insert(vec![signed(&c, NOW)], NOW, |_| true);
        assert!(accepted.is_empty() && writes.is_empty());
        assert_eq!(
            [seq(&table, &a), seq(&table, &b), seq(&table, &c)],
            [Some(NOW), Some(NOW), None]
        );
        let (accepted, writes) =
            table.insert(vec![signed(&a, NOW + 30_000)], NOW + 30_000, |_| true);
        assert_eq!(accepted.len(), 1);
        store.apply(writes)?;
        assert_eq!(seq(&table, &a), Some(NOW + 30_000));
        assert_eq!(read(&path, "lab")?.len(), 2);
        Ok(())
    }

    #[test]
    fn admission_order_conflicts_do_not_replace_another_device() -> Result<()> {
        let [own, a, b] = std::array::from_fn(|_| vibeface_secure::Identity::generate().unwrap());
        let dir = dir("admission-order-conflict");
        let path = dir.join("mesh.sqlite3");
        let (mut table, mut store) = Table::open(&path, &own.device_id(), "lab", NOW)?;
        store.apply(table.insert(vec![signed(&a, NOW)], NOW, |_| true).1)?;
        let (mut other, _) = Table::open(&dir.join("other.sqlite3"), &own.device_id(), "lab", NOW)?;
        let writes = other.insert(vec![signed(&b, NOW)], NOW, |_| true).1;
        assert!(store.apply(writes).is_err());
        let persisted = read(&path, "lab")?;
        assert_eq!(persisted.len(), 1);
        assert_eq!(persisted[0].record.device_id, a.device_id());
        assert_eq!(persisted[0].record.seq, NOW);
        Ok(())
    }

    #[test]
    fn a_cache_without_admission_order_is_rejected_without_changes() -> Result<()> {
        let dir = dir("unsupported-mesh-cache");
        let path = dir.join("mesh.sqlite3");
        let db = crate::store::private_file_connection(&path)?;
        db.execute_batch(
            "CREATE TABLE mesh_records(device_id TEXT PRIMARY KEY, seq INTEGER NOT NULL,
               expires_at INTEGER NOT NULL, first_seen_at INTEGER NOT NULL,
               last_seen_at INTEGER NOT NULL, reachable TEXT NOT NULL, record TEXT NOT NULL);
             INSERT INTO mesh_records VALUES('kept', 1, 2, 3, 4, 'unknown', '{}');",
        )?;
        drop(db);
        let before = std::fs::read(&path)?;
        let own = vibeface_secure::Identity::generate()?;
        let error = Table::open(&path, &own.device_id(), "lab", NOW)
            .err()
            .unwrap();
        assert!(format!("{error:#}").contains("admission_order"));
        assert_eq!(std::fs::read(&path)?, before);
        let db = Connection::open_with_flags(&path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        assert_eq!(
            db.query_row(
                "SELECT device_id, last_seen_at FROM mesh_records",
                [],
                |row| { Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?)) }
            )?,
            ("kept".to_owned(), 4)
        );
        assert_eq!(
            db.query_row(
                "SELECT COUNT(*) FROM sqlite_schema WHERE type = 'table'",
                [],
                |row| row.get::<_, i64>(0)
            )?,
            1
        );
        Ok(())
    }

    #[test]
    fn a_device_moves_forward_at_most_every_thirty_seconds() -> Result<()> {
        let own = vibeface_secure::Identity::generate()?;
        let peer = vibeface_secure::Identity::generate()?;
        let dir = dir("forward");
        let (mut table, _) = Table::open(&dir.join("mesh.sqlite3"), &own.device_id(), "lab", NOW)?;
        assert_eq!(insert(&mut table, vec![signed(&peer, NOW)], NOW), 1);
        assert_eq!(
            insert(&mut table, vec![signed(&peer, NOW - 1)], NOW + 60_000),
            0
        );
        assert_eq!(
            insert(&mut table, vec![signed(&peer, NOW + 10_000)], NOW + 40_000),
            0
        );
        assert_eq!(seq(&table, &peer), Some(NOW));
        // The first copy came late; the next is dated 30 s on and counts however soon it comes.
        assert_eq!(
            insert(&mut table, vec![signed(&peer, NOW + 30_000)], NOW + 1_000),
            1
        );
        assert_eq!(seq(&table, &peer), Some(NOW + 30_000));
        assert_eq!(
            insert(&mut table, vec![signed(&own, NOW + 40_000)], NOW + 40_000),
            0
        );
        // A device not held yet waits for `admit`; one held moves forward without it.
        let other = vibeface_secure::Identity::generate()?;
        let refused = table.insert(
            vec![signed(&other, NOW), signed(&peer, NOW + 60_000)],
            NOW + 60_000,
            |_| false,
        );
        assert_eq!(refused.0.len(), 1);
        assert_eq!(
            (seq(&table, &other), seq(&table, &peer)),
            (None, Some(NOW + 60_000))
        );
        Ok(())
    }

    /// A full table drops expired records first, then the record first seen most recently; the
    /// records known longest, protected ones and this Node's own stay.
    #[test]
    fn a_full_table_keeps_long_known_and_protected_records() -> Result<()> {
        let own = vibeface_secure::Identity::generate()?;
        let [a, b, c, d, e, member] =
            std::array::from_fn(|_| vibeface_secure::Identity::generate().unwrap());
        let dir = dir("full");
        let (mut table, _) = Table::open(&dir.join("mesh.sqlite3"), &own.device_id(), "lab", NOW)?;
        table.set_capacity(3);
        table.set_protected(HashSet::from([member.device_id()]));
        let ttl = u64::from(DEFAULT_TTL_S) * 1000;
        insert(&mut table, vec![signed(&a, NOW)], NOW);
        insert(&mut table, vec![signed(&member, NOW + 1)], NOW + 1);
        insert(&mut table, vec![signed(&b, NOW + ttl / 2)], NOW + ttl / 2);
        // a has expired: it goes first, though it was known longest.
        let later = NOW + ttl + 1;
        insert(&mut table, vec![signed(&c, later)], later);
        assert_eq!((seq(&table, &a), seq(&table, &c).is_some()), (None, true));
        // b (first seen before c) stays; c, the newest, gives way to d, and d to e.
        insert(&mut table, vec![signed(&d, later + 1)], later + 1);
        assert_eq!((seq(&table, &c), seq(&table, &d).is_some()), (None, true));
        // b moving forward keeps its first sighting.
        insert(&mut table, vec![signed(&b, later + 2)], later + 2);
        insert(&mut table, vec![signed(&e, later + 3)], later + 3);
        assert_eq!((seq(&table, &d), seq(&table, &e).is_some()), (None, true));
        assert!(seq(&table, &b).is_some() && seq(&table, &member).is_some());
        // Full of records that stay, a new one is refused.
        table.set_protected(HashSet::from([
            member.device_id(),
            b.device_id(),
            e.device_id(),
        ]));
        assert_eq!(
            insert(&mut table, vec![signed(&a, later + 4)], later + 4),
            0
        );
        assert_eq!(table.entries().count(), 3);
        Ok(())
    }

    #[test]
    fn records_outlive_a_restart_until_thirty_days_unseen() -> Result<()> {
        let own = vibeface_secure::Identity::generate()?;
        let [fresh, old] = std::array::from_fn(|_| vibeface_secure::Identity::generate().unwrap());
        let dir = dir("restart");
        let path = dir.join("mesh.sqlite3");
        let (mut table, mut store) = Table::open(&path, &own.device_id(), "lab", NOW)?;
        store.apply(table.insert(vec![signed(&old, NOW)], NOW, |_| true).1)?;
        let later = NOW + 20 * 24 * 3600 * 1000;
        store.apply(table.insert(vec![signed(&fresh, later)], later, |_| true).1)?;
        drop((table, store));
        let (restarted, _) = Table::open(
            &path,
            &own.device_id(),
            "lab",
            later + 15 * 24 * 3600 * 1000,
        )?;
        assert_eq!(seq(&restarted, &fresh), Some(later));
        assert_eq!(
            restarted.get(&fresh.device_id()).unwrap().first_seen_ms,
            later
        );
        assert_eq!(seq(&restarted, &old), None);
        assert_eq!(restarted.online(later + 1).count(), 1);
        drop(restarted);
        let (other, _) = Table::open(&path, &own.device_id(), "other", later)?;
        assert_eq!(other.entries().count(), 0);
        Ok(())
    }
}
