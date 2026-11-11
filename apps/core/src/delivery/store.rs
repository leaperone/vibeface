use super::types::{DeliveryOrder, Evidence, Mode, Role, StoredOrder, parse_word};
use alloy_primitives::U256;
use anyhow::{Context, Result, ensure};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use serde::{Deserialize, Serialize};
use std::{
    fs::File,
    io::{Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
};
use vibeface_bridge::platform::privatefs;
use vibeface_delivery::{WindowSpec, hash};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SpendIntent {
    pub id: String,
    pub chain_id: u64,
    pub wallet: String,
    pub to: String,
    pub calldata: String,
    pub value: String,
    pub gas_limit: u64,
    pub gas_cap: String,
    pub max_fee_per_gas: String,
    pub max_priority_fee_per_gas: String,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SignedTransaction {
    pub intent: SpendIntent,
    pub nonce: u64,
    pub raw_tx: Option<String>,
    pub tx_hash: Option<String>,
    pub phase: String,
    pub block_number: Option<u64>,
    pub block_hash: Option<String>,
    pub error: Option<String>,
}

pub struct ServiceStore {
    db: Connection,
    root: PathBuf,
    capacity: u64,
    max_orders: u32,
    admissions: (std::time::Instant, u32),
}
type TxRow = (
    String,
    u64,
    Option<String>,
    Option<String>,
    String,
    Option<u64>,
    Option<String>,
    Option<String>,
);

fn private_directory(path: &Path) -> Result<()> {
    if let Ok(metadata) = std::fs::symlink_metadata(path) {
        ensure!(
            metadata.is_dir() && !metadata.file_type().is_symlink(),
            "service directory must not be a symlink"
        );
    }
    crate::config::private_dir(path)
}
fn open_file(path: &Path, write: bool, create: bool) -> Result<File> {
    let file = match (write, create) {
        (true, true) => privatefs::open_or_create(path)?,
        (true, false) | (false, _) => privatefs::open_no_follow(path)?,
    };
    ensure!(
        file.metadata()?.is_file(),
        "service object is not a regular file"
    );
    Ok(file)
}
impl ServiceStore {
    pub fn open(data_dir: &Path, capacity: u64, max_orders: u32) -> Result<Self> {
        let root = data_dir.join("delivery");
        private_directory(&root)?;
        private_directory(&root.join("objects"))?;
        private_directory(&root.join("private"))?;
        let mut db = crate::store::private_file_connection(&root.join("service.sqlite3"))?;
        db.busy_timeout(std::time::Duration::from_secs(5))?;
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
                    "unversioned delivery database is not supported"
                );
                tx.execute_batch("PRAGMA auto_vacuum=FULL;")?;
                tx.execute_batch(include_str!("schema.sql"))?;
                tx.pragma_update(None, "user_version", crate::config::DATA_EPOCH)?;
            }
            crate::config::DATA_EPOCH => {}
            _ => anyhow::bail!(
                "unsupported delivery database schema version {version}; initialize a fresh Node data directory"
            ),
        }
        tx.commit()?;
        db.execute_batch("PRAGMA journal_mode=DELETE; PRAGMA synchronous=FULL;")?;
        Ok(Self {
            db,
            root,
            capacity,
            max_orders,
            admissions: (std::time::Instant::now(), 0),
        })
    }
    #[cfg(test)]
    pub(super) fn root(&self) -> &Path {
        &self.root
    }
    pub fn check_limits(&self, capacity: u64, max_orders: u32) -> Result<()> {
        check_capacity_limits(&self.db, capacity, max_orders)
    }
    fn atomic<T>(&mut self, run: impl FnOnce(&mut Self) -> Result<T>) -> Result<T> {
        self.db.execute_batch("SAVEPOINT delivery_write")?;
        match run(self) {
            Ok(value) => {
                self.db.execute_batch("RELEASE delivery_write")?;
                Ok(value)
            }
            Err(error) => {
                self.db
                    .execute_batch("ROLLBACK TO delivery_write; RELEASE delivery_write")?;
                Err(error)
            }
        }
    }
    fn external_admission(&mut self) -> Result<()> {
        if self.admissions.0.elapsed() >= std::time::Duration::from_secs(60) {
            self.admissions = (std::time::Instant::now(), 0);
        }
        ensure!(
            self.admissions.1 < 16,
            "public delivery admission rate exceeded"
        );
        self.admissions.1 += 1;
        Ok(())
    }
    fn records(&self) -> Result<Vec<(String, String)>> {
        let mut query = self
            .db
            .prepare("SELECT id,body FROM orders UNION ALL SELECT id,body FROM charity_orders")?;
        Ok(query
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
            .collect::<rusqlite::Result<Vec<_>>>()?)
    }
    fn usage(&self) -> Result<[(u32, u64); 3]> {
        capacity_usage(&self.db)
    }
    fn check_capacity(&self) -> Result<()> {
        self.check_limits(self.capacity, self.max_orders)
            .context("delivery storage or order capacity exhausted")
    }
    pub(super) fn reclaim_unaccepted_receivers(&mut self, at: u64, mode: Mode) -> Result<()> {
        for (id, body) in self.records()? {
            let owner: CapacityOwner = serde_json::from_str(&body)?;
            if owner.view.mode != mode {
                continue;
            }
            let record: serde_json::Value = serde_json::from_str(&body)?;
            let deadline = if owner.class() == 1
                && !owner.accepted
                && !owner.confirmed
                && owner.authorized_by.is_none()
            {
                record
                    .pointer("/order/terms/accept_until")
                    .or_else(|| record.pointer("/order/unsigned/terms/hold_deadline"))
            } else if owner.class() == 2 && owner.view.mode == Mode::Charity {
                record.pointer("/order/terms/retain_until")
            } else {
                continue;
            }
            .and_then(serde_json::Value::as_u64)
            .context("external delivery has no commitment deadline")?;
            if at <= deadline || self.has_effect_references(&id)? {
                continue;
            }
            self.remove_external_record(&id)?;
        }
        Ok(())
    }
    fn has_effect_references(&self, id: &str) -> Result<bool> {
        let direct = self.db.query_row("SELECT EXISTS(SELECT 1 FROM transactions WHERE substr(id,1,length(?1)+1)=?1||'/') OR EXISTS(SELECT 1 FROM private_values WHERE instr(id,?1)>0 AND id!='order-observation:'||?1)", [id], |row| row.get::<_, bool>(0))?;
        if direct {
            return Ok(true);
        }
        if super::budget::publication_has_order_reference(self, id)? {
            return Ok(true);
        }
        let Some(order) = self.find_order(id)? else {
            return Ok(false);
        };
        let Some(budget) = order.view.payment.budget_id else {
            return Ok(false);
        };
        Ok(self.db.query_row(
            "SELECT EXISTS(SELECT 1 FROM transactions WHERE substr(id,1,length(?1)+1)=?1||'/' AND (raw IS NULL OR phase NOT IN ('confirmed','failed','reverted','conflicted')))",
            [format!("budget-{budget}")],
            |row| row.get(0),
        )?)
    }
    pub(super) fn reclaim_unfunded_quote(&mut self, expected: &StoredOrder) -> Result<()> {
        let Some(current) = self.find_order(&expected.view.order_id)? else {
            return Ok(());
        };
        ensure!(
            serde_json::to_vec(&current)? == serde_json::to_vec(expected)?,
            "quotation changed during funding reconciliation"
        );
        ensure!(
            current.view.role == Role::Provider
                && !current.accepted
                && !current.confirmed
                && current.authorized_by.is_none()
                && current.network_id.is_none()
                && super::now() > current.order.unsigned.terms.retain_until,
            "provider quotation still has a service commitment"
        );
        if self.has_effect_references(&current.view.order_id)? {
            return Ok(());
        }
        self.remove_external_record(&current.view.order_id)
    }
    fn remove_external_record(&mut self, id: &str) -> Result<()> {
        self.remove_objects(id, false)?;
        self.atomic(|store| {
            store.db.execute(
                "DELETE FROM private_values WHERE id='order-observation:'||?1",
                [id],
            )?;
            store.db.execute("DELETE FROM orders WHERE id=?1", [id])?;
            store
                .db
                .execute("DELETE FROM charity_orders WHERE id=?1", [id])?;
            Ok(())
        })
    }
    pub(super) fn release_provider_objects(&mut self, id: &str) -> Result<()> {
        let order = self.order(id)?;
        if order.view.role != Role::Provider
            || !order.historical()
            || super::now() <= order.order.unsigned.terms.retain_until
        {
            return Ok(());
        }
        let budget = order
            .view
            .payment
            .budget_id
            .as_ref()
            .map(|id| format!("budget-{id}"));
        let pending: bool = self.db.query_row("SELECT EXISTS(SELECT 1 FROM transactions WHERE (substr(id,1,length(?1)+1)=?1||'/' OR substr(id,1,length(?2)+1)=?2||'/') AND (raw IS NULL OR phase NOT IN ('confirmed','failed','reverted','conflicted')))", params![id,budget], |row| row.get(0))?;
        if !pending && !super::budget::publication_retains_objects(self, id)? {
            self.remove_objects(id, true)?;
        }
        Ok(())
    }
    fn remove_objects(&mut self, id: &str, keep_capsules: bool) -> Result<()> {
        parse_word(id)?;
        let mut query = self.db.prepare(
            "SELECT idx,kind FROM objects WHERE order_id=?1 AND (?2=0 OR kind!='capsule')",
        )?;
        let objects = query
            .query_map(params![id, keep_capsules], |row| {
                Ok((row.get::<_, u16>(0)?, row.get::<_, String>(1)?))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        drop(query);
        for (index, kind) in objects {
            for partial in [true, false] {
                let path = self.object_path(id, index, &kind, partial)?;
                match std::fs::remove_file(path) {
                    Ok(()) => {}
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                    Err(error) => return Err(error.into()),
                }
            }
        }
        self.db.execute(
            "DELETE FROM objects WHERE order_id=?1 AND (?2=0 OR kind!='capsule')",
            params![id, keep_capsules],
        )?;
        match std::fs::remove_dir(self.root.join("objects").join(id)) {
            Ok(()) => {}
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::NotFound | std::io::ErrorKind::DirectoryNotEmpty
                ) => {}
            Err(error) => return Err(error.into()),
        }
        Ok(())
    }
    pub fn set_limits(&mut self, capacity: u64, max_orders: u32) {
        self.capacity = capacity;
        self.max_orders = max_orders;
    }
    pub fn reserve_proof(&self, digest: &str) -> Result<PathBuf> {
        parse_word(digest)?;
        let path = self
            .root
            .join("private")
            .join(format!("proof-{digest}.receipt"));
        let prior: Option<String> = self
            .db
            .query_row(
                "SELECT path FROM proof_jobs WHERE digest=?1",
                [digest],
                |r| r.get(0),
            )
            .optional()?;
        if let Some(prior) = prior {
            ensure!(Path::new(&prior) == path, "proof job path changed");
            return Ok(path);
        }
        let used = self.usage()?.iter().map(|usage| usage.1).sum::<u64>();
        ensure!(
            used.checked_add(16 * 1024)
                .is_some_and(|used| used <= self.capacity),
            "proof receipt storage quota exceeded"
        );
        let tx = self.db.unchecked_transaction()?;
        tx.execute(
            "INSERT INTO proof_jobs(digest,path,size) VALUES(?1,?2,16384)",
            params![digest, path.to_str().context("invalid receipt path")?],
        )?;
        check_physical_capacity(&tx, self.capacity)?;
        tx.commit()?;
        Ok(path)
    }
    pub fn order(&self, id: &str) -> Result<StoredOrder> {
        parse_word(id)?;
        let body: String = self
            .db
            .query_row("SELECT body FROM orders WHERE id=?1", [id], |r| r.get(0))
            .context("delivery order not found")?;
        self.decode_order(&body)
    }
    fn decode_order(&self, body: &str) -> Result<StoredOrder> {
        decode_order(&self.db, body)
    }
    pub fn find_order(&self, id: &str) -> Result<Option<StoredOrder>> {
        parse_word(id)?;
        let exists: bool = self.db.query_row(
            "SELECT EXISTS(SELECT 1 FROM orders WHERE id=?1)",
            [id],
            |r| r.get(0),
        )?;
        if exists {
            self.order(id).map(Some)
        } else {
            Ok(None)
        }
    }
    pub fn save_order(&mut self, order: &StoredOrder) -> Result<()> {
        parse_word(&order.view.order_id)?;
        let encoded = serde_json::to_string(order)?;
        ensure!(
            encoded.len() <= MAX_ORDER_BYTES,
            "delivery order exceeds its control bound"
        );
        let role = serde_json::to_string(&order.view.role)?;
        let existing: Option<(Option<String>, String)> = self
            .db
            .query_row(
                "SELECT network,role FROM orders WHERE id=?1",
                [&order.view.order_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        if let Some((network, prior_role)) = &existing {
            ensure!(
                network == &order.network_id && &role == prior_role,
                "delivery order owner cannot change"
            );
        } else if serde_json::from_str::<CapacityOwner>(&encoded)?.class() != 0 {
            self.reclaim_unaccepted_receivers(super::now(), Mode::Paid)?;
            self.external_admission()?;
        }
        self.atomic(|store| {
            store.db.execute("INSERT INTO orders(id,network,intent,role,body) VALUES(?1,?2,?3,?4,?5) ON CONFLICT(id) DO UPDATE SET body=excluded.body",
                params![order.view.order_id,order.network_id,order.view.intent_id,role,encoded])?;
            if order.chain_observed_at_ms > 0 || order.settlement_complete {
                store.db.execute("INSERT INTO private_values(id,body) VALUES(?1,?2) ON CONFLICT(id) DO UPDATE SET body=excluded.body",
                    params![format!("order-observation:{}",order.view.order_id),serde_json::to_vec(&(order.chain_observed_at_ms,order.settlement_complete))?])?;
            }
            if existing.is_none() {
                store.reserve_windows(&order.view.order_id, &order.order.unsigned.windows)?;
            }
            store.check_capacity()
        })
    }
    pub fn by_intent(&self, network: &str, intent: &str) -> Result<Option<StoredOrder>> {
        let body: Option<String> = self
            .db
            .query_row(
                "SELECT body FROM orders WHERE network=?1 AND intent=?2 AND role='\"sender\"'",
                params![network, intent],
                |r| r.get(0),
            )
            .optional()?;
        body.map(|body| self.decode_order(&body)).transpose()
    }
    pub(super) fn save_authorized_batch(&mut self, orders: &[StoredOrder]) -> Result<()> {
        ensure!(
            (1..=2).contains(&orders.len()),
            "invalid confirmed delivery batch"
        );
        let tx = self
            .db
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        for order in orders {
            ensure!(
                (order.confirmed && order.view.role == super::types::Role::Sender)
                    || (order.accepted && order.view.role == super::types::Role::Receiver),
                "batch contains no explicit sender/receiver authorization"
            );
            let changed = tx.execute(
                "UPDATE orders SET body=?1 WHERE id=?2 AND network IS ?3 AND role=?4",
                params![
                    serde_json::to_string(order)?,
                    order.view.order_id,
                    order.network_id,
                    serde_json::to_string(&order.view.role)?
                ],
            )?;
            ensure!(changed == 1, "confirmed batch order owner changed");
        }
        tx.commit()?;
        Ok(())
    }
    pub fn orders(&self, network: Option<&str>) -> Result<Vec<StoredOrder>> {
        let mut statement = self
            .db
            .prepare("SELECT body FROM orders WHERE network IS ?1 ORDER BY rowid DESC LIMIT ?2")?;
        let bodies = statement
            .query_map(params![network, self.max_orders], |r| r.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        bodies
            .into_iter()
            .map(|body| self.decode_order(&body))
            .collect()
    }
    pub(super) fn progress_orders(&self, network: Option<&str>) -> Result<Vec<StoredOrder>> {
        let sql = format!(
            "SELECT body FROM orders WHERE network IS ?1 AND NOT ({HISTORICAL_ORDER}) ORDER BY json_extract(body,'$.view.updated_at_ms') LIMIT ?2"
        );
        let mut query = self.db.prepare(&sql)?;
        let mut bodies = query
            .query_map(params![network, self.max_orders], |row| {
                row.get::<_, String>(0)
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let sql = format!(
            "SELECT body FROM orders WHERE network IS ?1 AND ({HISTORICAL_ORDER}) AND json_extract((SELECT CAST(body AS TEXT) FROM private_values WHERE id='order-observation:'||orders.id),'$[0]')<?2 ORDER BY json_extract((SELECT CAST(body AS TEXT) FROM private_values WHERE id='order-observation:'||orders.id),'$[0]') LIMIT 1"
        );
        let mut query = self.db.prepare(&sql)?;
        bodies.extend(
            query
                .query_map(
                    params![network, crate::runtime::now_ms().saturating_sub(60_000)],
                    |row| row.get::<_, String>(0),
                )?
                .collect::<rusqlite::Result<Vec<_>>>()?,
        );
        bodies
            .into_iter()
            .map(|body| self.decode_order(&body))
            .collect()
    }
    pub fn save_charity_order(
        &mut self,
        id: &str,
        network: Option<&str>,
        intent: &str,
        body: &str,
        windows: &[WindowSpec],
    ) -> Result<()> {
        parse_word(id)?;
        ensure!(
            body.len() <= MAX_ORDER_BYTES,
            "charity order exceeds its control bound"
        );
        let owner: CapacityOwner = serde_json::from_str(body)?;
        let prior: Option<Option<String>> = self
            .db
            .query_row(
                "SELECT network FROM charity_orders WHERE id=?1",
                [id],
                |row| row.get(0),
            )
            .optional()?;
        if let Some(prior) = &prior {
            ensure!(
                prior.as_deref() == network,
                "charity order owner cannot change"
            );
        } else if owner.class() != 0 {
            self.reclaim_unaccepted_receivers(super::now(), Mode::Charity)?;
            self.external_admission()?;
        }
        self.atomic(|store| {
            store.db.execute("INSERT INTO charity_orders(id,network,intent,body) VALUES(?1,?2,?3,?4) ON CONFLICT(id) DO UPDATE SET body=excluded.body",params![id,network,intent,body])?;
            if prior.is_none() {
                store.reserve_windows(id, windows)?;
            }
            store.check_capacity()
        })
    }
    fn reserve_windows(&mut self, id: &str, windows: &[WindowSpec]) -> Result<()> {
        for window in windows {
            self.reserve_object_record(
                id,
                window.index,
                "ciphertext",
                &hex::encode(window.ciphertext_hash),
                window.ciphertext_bytes,
            )?;
            self.reserve_object_record(
                id,
                window.index,
                "capsule",
                &hex::encode(window.capsule_hash),
                80,
            )?;
        }
        Ok(())
    }
    pub fn charity_order(&self, id: &str) -> Result<Option<String>> {
        parse_word(id)?;
        Ok(self
            .db
            .query_row("SELECT body FROM charity_orders WHERE id=?1", [id], |r| {
                r.get(0)
            })
            .optional()?)
    }
    pub fn charity_by_intent(&self, network: &str, intent: &str) -> Result<Option<String>> {
        Ok(self
            .db
            .query_row(
                "SELECT body FROM charity_orders WHERE network=?1 AND intent=?2",
                params![network, intent],
                |r| r.get(0),
            )
            .optional()?)
    }
    pub fn charity_orders(&self, network: Option<&str>) -> Result<Vec<String>> {
        let mut query = self.db.prepare(
            "SELECT body FROM charity_orders WHERE network IS ?1 ORDER BY rowid DESC LIMIT ?2",
        )?;
        Ok(query
            .query_map(params![network, self.max_orders], |r| r.get(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?)
    }
    pub fn put_value(&self, id: &str, value: &[u8]) -> Result<()> {
        self.put_values(&[(id, value)])
    }
    pub fn put_values(&self, values: &[(&str, &[u8])]) -> Result<()> {
        ensure!(
            !values.is_empty() && values.len() <= 8,
            "invalid private delivery batch"
        );
        let mut seen = std::collections::BTreeSet::new();
        let tx = self.db.unchecked_transaction()?;
        for (id, value) in values {
            ensure!(
                id.len() <= 160 && value.len() <= 1024 * 1024 && seen.insert(*id),
                "invalid or duplicate private delivery value"
            );
            tx.execute("INSERT INTO private_values(id,body) VALUES(?1,?2) ON CONFLICT(id) DO UPDATE SET body=excluded.body",params![id,value])?;
        }
        check_capacity_limits(&tx, self.capacity, self.max_orders)?;
        tx.commit()?;
        Ok(())
    }
    pub fn value(&self, id: &str) -> Result<Option<Vec<u8>>> {
        Ok(self
            .db
            .query_row("SELECT body FROM private_values WHERE id=?1", [id], |r| {
                r.get(0)
            })
            .optional()?)
    }
    pub fn values_with_prefix(&self, prefix: &str) -> Result<Vec<Vec<u8>>> {
        let mut query = self.db.prepare(
            "SELECT body FROM private_values WHERE substr(id,1,length(?1))=?1 ORDER BY id",
        )?;
        Ok(query
            .query_map([prefix], |row| row.get(0))?
            .collect::<rusqlite::Result<_>>()?)
    }
    fn object_path(&self, order: &str, index: u16, kind: &str, partial: bool) -> Result<PathBuf> {
        parse_word(order)?;
        ensure!(
            index < 16 && matches!(kind, "ciphertext" | "capsule"),
            "invalid delivery object path"
        );
        let parent = self.root.join("objects").join(order);
        if let Ok(metadata) = std::fs::symlink_metadata(&parent) {
            ensure!(
                metadata.is_dir() && !metadata.file_type().is_symlink(),
                "service object directory must not be a symlink"
            );
        }
        Ok(parent.join(format!(
            "{index}-{kind}{}",
            if partial { ".part" } else { ".bin" }
        )))
    }
    pub fn reserve_object(
        &mut self,
        order: &str,
        index: u16,
        kind: &str,
        digest: &str,
        size: u32,
    ) -> Result<u32> {
        let committed = self.atomic(|store| {
            let committed = store.reserve_object_record(order, index, kind, digest, size)?;
            store.check_capacity()?;
            Ok(committed)
        })?;
        let path = self.object_path(order, index, kind, false)?;
        if path.exists() {
            let mut bytes = Vec::new();
            open_file(&path, false, false)?
                .take(u64::from(size) + 1)
                .read_to_end(&mut bytes)?;
            ensure!(
                bytes.len() == size as usize && hex::encode(hash(&bytes)) == digest,
                "committed delivery object is corrupt"
            );
            self.db.execute("UPDATE objects SET committed=size,complete=1 WHERE order_id=?1 AND idx=?2 AND kind=?3",params![order,index,kind])?;
            return Ok(size);
        }
        Ok(committed)
    }
    fn reserve_object_record(
        &self,
        order: &str,
        index: u16,
        kind: &str,
        digest: &str,
        size: u32,
    ) -> Result<u32> {
        parse_word(order)?;
        parse_word(digest)?;
        ensure!(
            index < 16 && matches!(kind, "ciphertext" | "capsule"),
            "invalid delivery object path"
        );
        ensure!(
            size > 0 && size <= if kind == "capsule" { 80 } else { 65_604 },
            "delivery object exceeds window limit"
        );
        let prior: Option<(String, u32, u32)> = self
            .db
            .query_row(
                "SELECT hash,size,committed FROM objects WHERE order_id=?1 AND idx=?2 AND kind=?3",
                params![order, index, kind],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()?;
        if let Some((old_hash, old_size, committed)) = prior {
            ensure!(
                old_hash == digest && old_size == size,
                "delivery object changed across retry"
            );
            return Ok(committed);
        }
        self.db.execute(
            "INSERT INTO objects(order_id,idx,kind,hash,size) VALUES(?1,?2,?3,?4,?5)",
            params![order, index, kind, digest, size],
        )?;
        Ok(0)
    }
    pub fn append_object(
        &mut self,
        order: &str,
        index: u16,
        kind: &str,
        offset: u32,
        bytes: &[u8],
    ) -> Result<u32> {
        ensure!(
            !bytes.is_empty() && bytes.len() <= super::types::CHUNK_BYTES,
            "invalid delivery chunk length"
        );
        let (digest,size,committed,complete): (String,u32,u32,bool) = self.db.query_row("SELECT hash,size,committed,complete FROM objects WHERE order_id=?1 AND idx=?2 AND kind=?3",params![order,index,kind],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?))).context("object requires a reservation")?;
        ensure!(
            offset == committed,
            "delivery chunk does not start at the durable prefix"
        );
        ensure!(
            !complete && u64::from(offset) + bytes.len() as u64 <= u64::from(size),
            "delivery chunk exceeds reservation"
        );
        let partial = self.object_path(order, index, kind, true)?;
        private_directory(partial.parent().context("object directory missing")?)?;
        let mut file = open_file(&partial, true, true)?;
        ensure!(
            file.metadata()?.len() >= u64::from(committed),
            "durable delivery prefix is missing"
        );
        file.set_len(u64::from(committed))?;
        file.seek(SeekFrom::Start(u64::from(committed)))?;
        file.write_all(bytes)?;
        file.sync_all()?;
        let next = committed + bytes.len() as u32;
        if next == size {
            let mut data = Vec::new();
            file.seek(SeekFrom::Start(0))?;
            file.read_to_end(&mut data)?;
            if hex::encode(hash(&data)) != digest {
                file.set_len(0)?;
                file.sync_all()?;
                self.db.execute(
                    "UPDATE objects SET committed=0 WHERE order_id=?1 AND idx=?2 AND kind=?3",
                    params![order, index, kind],
                )?;
                anyhow::bail!("complete delivery object hash mismatch");
            }
            drop(file);
            std::fs::rename(&partial, self.object_path(order, index, kind, false)?)?;
            privatefs::sync_dir(partial.parent().context("object directory missing")?)?;
        }
        self.db.execute(
            "UPDATE objects SET committed=?4,complete=?5 WHERE order_id=?1 AND idx=?2 AND kind=?3",
            params![order, index, kind, next, next == size],
        )?;
        Ok(next)
    }
    pub fn write_object(
        &mut self,
        order: &str,
        index: u16,
        kind: &str,
        bytes: &[u8],
    ) -> Result<()> {
        let digest = hex::encode(hash(bytes));
        let mut offset = self.reserve_object(order, index, kind, &digest, bytes.len() as u32)?;
        while (offset as usize) < bytes.len() {
            let end = (offset as usize + super::types::CHUNK_BYTES).min(bytes.len());
            offset =
                self.append_object(order, index, kind, offset, &bytes[offset as usize..end])?;
        }
        Ok(())
    }
    pub fn object(&self, order: &str, index: u16, kind: &str) -> Result<Vec<u8>> {
        let (digest, size, complete): (String, u32, bool) = self.db.query_row(
            "SELECT hash,size,complete FROM objects WHERE order_id=?1 AND idx=?2 AND kind=?3",
            params![order, index, kind],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )?;
        ensure!(complete, "delivery object is not complete");
        let mut bytes = Vec::new();
        open_file(&self.object_path(order, index, kind, false)?, false, false)?
            .take(u64::from(size) + 1)
            .read_to_end(&mut bytes)?;
        ensure!(
            bytes.len() == size as usize && hex::encode(hash(&bytes)) == digest,
            "delivery object integrity failed"
        );
        Ok(bytes)
    }
    pub fn object_complete(&self, order: &str, index: u16, kind: &str) -> Result<bool> {
        Ok(self
            .db
            .query_row(
                "SELECT complete FROM objects WHERE order_id=?1 AND idx=?2 AND kind=?3",
                params![order, index, kind],
                |r| r.get(0),
            )
            .optional()?
            .unwrap_or(false))
    }
    #[cfg(test)]
    fn reserve_transaction(
        &mut self,
        intent: &SpendIntent,
        pending_nonce: u64,
    ) -> Result<SignedTransaction> {
        self.reserve_transaction_inner(intent, pending_nonce, None, None, |_, _| Ok(None))
    }
    #[cfg(test)]
    pub(super) fn reserve_budget_transaction(
        &mut self,
        intent: &SpendIntent,
        pending_nonce: u64,
        budget_group: &str,
        total_gas_cap: &str,
    ) -> Result<SignedTransaction> {
        self.reserve_transaction_inner(
            intent,
            pending_nonce,
            Some((budget_group, total_gas_cap)),
            None,
            |_, _| Ok(None),
        )
    }
    pub fn signed_budget_transaction(
        &mut self,
        intent: &SpendIntent,
        pending_nonce: u64,
        budget_group: &str,
        total_gas_cap: &str,
        sign: impl FnOnce(&SpendIntent, u64) -> Result<(String, String)>,
    ) -> Result<SignedTransaction> {
        self.reserve_transaction_inner(
            intent,
            pending_nonce,
            Some((budget_group, total_gas_cap)),
            None,
            |intent, nonce| sign(intent, nonce).map(Some),
        )
    }
    pub(super) fn signed_publication_transaction(
        &mut self,
        intent: &SpendIntent,
        pending_nonce: u64,
        budget_group: &str,
        total_gas_cap: &str,
        claim: (&str, &[u8]),
        sign: impl FnOnce(&SpendIntent, u64) -> Result<(String, String)>,
    ) -> Result<SignedTransaction> {
        ensure!(
            budget_group.starts_with("budget-")
                && intent.id.starts_with(&format!("{budget_group}/reveal-")),
            "publication attempt has no protected budget"
        );
        self.reserve_transaction_inner(
            intent,
            pending_nonce,
            Some((budget_group, total_gas_cap)),
            Some(claim),
            |intent, nonce| sign(intent, nonce).map(Some),
        )
    }
    fn reserve_transaction_inner(
        &mut self,
        intent: &SpendIntent,
        pending_nonce: u64,
        budget: Option<(&str, &str)>,
        claim: Option<(&str, &[u8])>,
        sign: impl FnOnce(&SpendIntent, u64) -> Result<Option<(String, String)>>,
    ) -> Result<SignedTransaction> {
        ensure!(
            !intent.id.is_empty()
                && intent.id.len() <= 160
                && intent.chain_id <= i64::MAX as u64
                && pending_nonce < i64::MAX as u64,
            "invalid transaction identity"
        );
        let encoded = serde_json::to_string(intent)?;
        let tx = self
            .db
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let usage = capacity_usage(&tx)?;
        let used = usage.iter().map(|usage| usage.1).sum::<u64>();
        let class = transaction_class(&tx, &intent.id)?;
        let class_room = if class == 0 {
            self.capacity
        } else {
            self.capacity / 4
        };
        let prior:Option<TxRow>=tx.query_row("SELECT intent,nonce,raw,hash,phase,block_number,block_hash,error FROM transactions WHERE id=?1",[&intent.id],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?,r.get(5)?,r.get(6)?,r.get(7)?))).optional()?;
        let hole: Option<String> = tx.query_row("SELECT id FROM transactions WHERE chain=?1 AND wallet=?2 AND raw IS NULL ORDER BY nonce LIMIT 1",params![intent.chain_id,intent.wallet],|row|row.get(0)).optional()?;
        if let Some((old, nonce, raw_tx, tx_hash, phase, block_number, block_hash, error)) = prior {
            ensure!(old == encoded, "transaction intent changed across retry");
            let mut transaction = SignedTransaction {
                intent: intent.clone(),
                nonce,
                raw_tx,
                tx_hash,
                phase,
                block_number,
                block_hash,
                error,
            };
            if transaction.raw_tx.is_none() {
                ensure!(
                    hole.as_deref() == Some(intent.id.as_str()),
                    "recover the earliest unsigned delivery intent before another spend"
                );
                if let Some((raw, hash)) = sign(intent, nonce)? {
                    check_signed_bytes(&raw, &hash)?;
                    ensure!(
                        used + raw.len() as u64 <= self.capacity
                            && usage[class].1 + raw.len() as u64 <= class_room,
                        "signed delivery transaction storage quota exceeded"
                    );
                    tx.execute("UPDATE transactions SET raw=?2,hash=?3,phase='signed',error=NULL WHERE id=?1",params![intent.id,raw,hash])?;
                    transaction.raw_tx = Some(raw);
                    transaction.tx_hash = Some(hash);
                    transaction.phase = "signed".into();
                    transaction.error = None;
                }
            }
            if let Some(claim) = claim {
                bind_publication_attempt(&tx, intent, claim)?;
            }
            check_capacity_limits(&tx, self.capacity, self.max_orders)?;
            tx.commit()?;
            return Ok(transaction);
        }
        ensure!(
            hole.is_none(),
            "recover the earliest unsigned delivery intent before reserving another nonce"
        );
        if let Some((group, cap)) = budget {
            ensure!(
                !group.is_empty()
                    && group.len() <= 96
                    && group
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b"-_".contains(&b))
                    && intent.id.starts_with(&format!("{group}/")),
                "invalid transaction budget group"
            );
            let previous: Option<String> = tx
                .query_row(
                    "SELECT cap FROM gas_budgets WHERE chain=?1 AND wallet=?2 AND budget_group=?3",
                    params![intent.chain_id, intent.wallet, group],
                    |r| r.get(0),
                )
                .optional()?;
            ensure!(
                previous.as_deref().is_none_or(|old| old == cap),
                "authorized transaction gas budget changed"
            );
            let total = U256::from_str_radix(cap, 10)?;
            let cost = |value: &SpendIntent| -> Result<U256> {
                U256::from(value.gas_limit)
                    .checked_mul(U256::from_str_radix(&value.max_fee_per_gas, 10)?)
                    .context("transaction gas budget overflow")
            };
            let mut spent = cost(intent)?;
            let mut query=tx.prepare("SELECT intent FROM transactions WHERE chain=?1 AND wallet=?2 AND substr(id,1,length(?3)+1)=?3||'/'")?;
            let values = query
                .query_map(params![intent.chain_id, intent.wallet, group], |r| {
                    r.get::<_, String>(0)
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            for encoded in values {
                spent = spent
                    .checked_add(cost(&serde_json::from_str(&encoded)?)?)
                    .context("transaction gas budget overflow")?;
            }
            ensure!(
                spent <= total,
                "order transactions exceed the authorized total gas budget"
            );
            drop(query);
            tx.execute("INSERT INTO gas_budgets(chain,wallet,budget_group,cap) VALUES(?1,?2,?3,?4) ON CONFLICT DO NOTHING",params![intent.chain_id,intent.wallet,group,cap])?;
        }
        let last: Option<u64> = tx.query_row(
            "SELECT max(nonce) FROM transactions WHERE chain=?1 AND wallet=?2",
            params![intent.chain_id, intent.wallet],
            |r| r.get(0),
        )?;
        let nonce = pending_nonce.max(last.map(|n| n + 1).unwrap_or(0));
        ensure!(nonce < i64::MAX as u64, "transaction nonce exhausted");
        let signed = sign(intent, nonce)?;
        if let Some((raw, hash)) = &signed {
            check_signed_bytes(raw, hash)?;
        }
        let (raw_tx, tx_hash, phase) = match signed {
            Some((raw, hash)) => (Some(raw), Some(hash), "signed"),
            None => (None, None, "reserved"),
        };
        ensure!(
            used + encoded.len() as u64 + raw_tx.as_ref().map_or(0, |raw| raw.len() as u64) + 1024
                <= self.capacity,
            "signed delivery transaction storage quota exceeded"
        );
        ensure!(
            usage[class].1
                + encoded.len() as u64
                + raw_tx.as_ref().map_or(0, |raw| raw.len() as u64)
                + 1024
                <= class_room,
            "public transaction storage quota exceeded"
        );
        tx.execute("INSERT INTO transactions(id,chain,wallet,nonce,intent,raw,hash,phase) VALUES(?1,?2,?3,?4,?5,?6,?7,?8)",params![intent.id,intent.chain_id,intent.wallet,nonce,encoded,raw_tx,tx_hash,phase])?;
        if let Some(claim) = claim {
            bind_publication_attempt(&tx, intent, claim)?;
        }
        check_capacity_limits(&tx, self.capacity, self.max_orders)?;
        tx.commit()?;
        Ok(SignedTransaction {
            intent: intent.clone(),
            nonce,
            raw_tx,
            tx_hash,
            phase: phase.into(),
            block_number: None,
            block_hash: None,
            error: None,
        })
    }
    #[cfg(test)]
    pub(super) fn sign_transaction(&mut self, id: &str, raw: &str, hash: &str) -> Result<()> {
        let transaction = self.transaction(id)?;
        self.reserve_transaction_inner(
            &transaction.intent,
            transaction.nonce,
            None,
            None,
            |_, _| Ok(Some((raw.to_owned(), hash.to_owned()))),
        )?;
        Ok(())
    }
    pub fn observe_transaction(
        &self,
        id: &str,
        phase: &str,
        block_number: Option<u64>,
        block_hash: Option<&str>,
        error: Option<&str>,
    ) -> Result<()> {
        ensure!(
            matches!(
                phase,
                "reserved"
                    | "signed"
                    | "submitted"
                    | "included"
                    | "confirming"
                    | "confirmed"
                    | "reorged"
                    | "reverted"
                    | "unknown"
                    | "failed"
                    | "conflicted"
            ),
            "invalid transaction phase"
        );
        self.db.execute(
            "UPDATE transactions SET phase=?2,block_number=?3,block_hash=?4,error=?5,checked_at=?6 WHERE id=?1",
            params![id, phase, block_number, block_hash, error,crate::runtime::now_ms()],
        )?;
        Ok(())
    }
    pub fn transaction(&self, id: &str) -> Result<SignedTransaction> {
        let (encoded,nonce,raw_tx,tx_hash,phase,block_number,block_hash,error):TxRow=self.db.query_row("SELECT intent,nonce,raw,hash,phase,block_number,block_hash,error FROM transactions WHERE id=?1",[id],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?,r.get(5)?,r.get(6)?,r.get(7)?)))?;
        Ok(SignedTransaction {
            intent: serde_json::from_str(&encoded)?,
            nonce,
            raw_tx,
            tx_hash,
            phase,
            block_number,
            block_hash,
            error,
        })
    }
    pub fn find_transaction(&self, id: &str) -> Result<Option<SignedTransaction>> {
        let exists: bool = self.db.query_row(
            "SELECT EXISTS(SELECT 1 FROM transactions WHERE id=?1)",
            [id],
            |r| r.get(0),
        )?;
        if exists {
            self.transaction(id).map(Some)
        } else {
            Ok(None)
        }
    }
    pub fn next_transaction_for_reconcile(&self) -> Result<Option<SignedTransaction>> {
        let id:Option<String>=self.db.query_row("SELECT id FROM transactions WHERE id NOT LIKE 'budget-%/reveal-%' AND phase NOT IN ('failed','reverted','conflicted') AND (phase!='confirmed' OR checked_at<?1) ORDER BY phase='confirmed',checked_at,nonce LIMIT 1",[crate::runtime::now_ms().saturating_sub(60_000)],|r|r.get(0)).optional()?;
        id.map(|id| self.transaction(&id)).transpose()
    }
    pub(super) fn next_nonce_is_contiguous(
        &self,
        chain: u64,
        wallet: &str,
        nonce: u64,
    ) -> Result<bool> {
        let last: Option<u64> = self.db.query_row(
            "SELECT max(nonce) FROM transactions WHERE chain=?1 AND wallet=?2",
            params![chain, wallet],
            |row| row.get(0),
        )?;
        Ok(last.is_none_or(|last| last < nonce))
    }
    #[cfg(test)]
    pub(super) fn recent_transactions(&self) -> Result<Vec<SignedTransaction>> {
        let mut query = self
            .db
            .prepare("SELECT id FROM transactions ORDER BY rowid DESC LIMIT 128")?;
        let ids = query
            .query_map([], |r| r.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        ids.iter().map(|id| self.transaction(id)).collect()
    }
}
fn decode_order(db: &Connection, body: &str) -> Result<StoredOrder> {
    let mut order: StoredOrder = serde_json::from_str(body)?;
    let observation: Option<Vec<u8>> = db
        .query_row(
            "SELECT body FROM private_values WHERE id=?1",
            [format!("order-observation:{}", order.view.order_id)],
            |row| row.get(0),
        )
        .optional()?;
    if let Some(bytes) = observation {
        let (observed, complete): (i64, bool) = serde_json::from_slice(&bytes)?;
        order.chain_observed_at_ms = observed;
        order.settlement_complete = complete;
    }
    Ok(order)
}
fn order_class(db: &Connection, id: &str) -> Result<Option<usize>> {
    let body: Option<String> = db.query_row(
        "SELECT body FROM orders WHERE id=?1 UNION ALL SELECT body FROM charity_orders WHERE id=?1",
        [id], |row| row.get(0),
    ).optional()?;
    body.map(|body| Ok(serde_json::from_str::<CapacityOwner>(&body)?.class()))
        .transpose()
}
fn budget_class(db: &Connection, id: &str) -> Result<usize> {
    parse_word(id)?;
    let body: Vec<u8> = db
        .query_row(
            "SELECT body FROM private_values WHERE id=?1",
            [format!("protected-budget:{id}")],
            |row| row.get(0),
        )
        .context("protected budget has no capacity owner")?;
    let record: super::budget::BudgetRecord = serde_json::from_slice(&body)?;
    ensure!(
        record.terms.budget_id == id,
        "protected budget capacity identity changed"
    );
    match record.role {
        Role::Sender => Ok(0),
        Role::Provider => Ok(2),
        Role::Receiver => {
            let mut query = db.prepare(
                "SELECT body FROM orders WHERE json_extract(body,'$.view.payment.budget_id')=?1",
            )?;
            let mut class = None;
            for body in query.query_map([id], |row| row.get::<_, String>(0))? {
                let owner: CapacityOwner = serde_json::from_str(&body?)?;
                ensure!(
                    owner.view.role == Role::Receiver,
                    "budget capacity role differs from its orders"
                );
                class = Some(class.unwrap_or(0).max(owner.class()));
            }
            Ok(class.unwrap_or(1))
        }
    }
}
fn transaction_class(db: &Connection, id: &str) -> Result<usize> {
    let group = id.split('/').next().unwrap_or("");
    if let Some(budget) = group.strip_prefix("budget-") {
        budget_class(db, budget)
    } else {
        Ok(order_class(db, group)?.unwrap_or(0))
    }
}
fn private_class(db: &Connection, id: &str, body: &[u8]) -> Result<usize> {
    if let Some(order) = id.strip_prefix("order-observation:") {
        return Ok(order_class(db, order)?.unwrap_or(0));
    }
    for prefix in ["protected-budget:", "protected-flush:"] {
        if let Some(budget) = id.strip_prefix(prefix) {
            return budget_class(db, budget);
        }
    }
    for prefix in ["protected-hold:", "protected-claim-window:"] {
        if let Some(window) = id.strip_prefix(prefix) {
            let order = window
                .split_once(':')
                .context("protected window has no order")?
                .0;
            return order_class(db, order)?.context("protected window has no capacity owner");
        }
    }
    if id.starts_with("protected-claim:") {
        let claim: serde_json::Value = serde_json::from_slice(body)?;
        return budget_class(
            db,
            claim
                .pointer("/terms/budget_id")
                .and_then(serde_json::Value::as_str)
                .context("publication claim has no budget capacity owner")?,
        );
    }
    if let Some(intent) = id.strip_prefix("local-spend:budget-") {
        return budget_class(db, intent.split('/').next().unwrap_or(""));
    }
    Ok(0)
}
fn capacity_usage(db: &Connection) -> Result<[(u32, u64); 3]> {
    let mut usage = [(0, 0); 3];
    let mut owners = std::collections::HashMap::new();
    let mut query =
        db.prepare("SELECT id,body FROM orders UNION ALL SELECT id,body FROM charity_orders")?;
    for row in query.query_map([], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
    })? {
        let (id, body) = row?;
        let owner: CapacityOwner = serde_json::from_str(&body)?;
        let class = owner.class();
        let historical = if owner.view.mode == Mode::Paid {
            decode_order(db, &body)?.historical()
        } else {
            owner.view.evidence == Evidence::CharityReceipt
                && (owner.view.role != Role::Receiver || owner.notified)
        };
        usage[class].0 += u32::from(!historical);
        usage[class].1 += body.len() as u64 + id.len() as u64 + 256;
        owners.insert(id, class);
    }
    let mut query = db.prepare("SELECT order_id,size FROM objects")?;
    for row in query.query_map([], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, u64>(1)?))
    })? {
        let (id, bytes) = row?;
        usage[*owners.get(&id).unwrap_or(&0)].1 += bytes + 256;
    }
    let mut query = db.prepare("SELECT id,CASE WHEN substr(id,1,16)='protected-claim:' THEN body END,length(id)+length(body)+128 FROM private_values")?;
    for row in query.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, Option<Vec<u8>>>(1)?,
            row.get::<_, u64>(2)?,
        ))
    })? {
        let (id, body, bytes) = row?;
        usage[private_class(db, &id, body.as_deref().unwrap_or_default())?].1 += bytes;
    }
    let proofs: u64 = db.query_row(
        "SELECT coalesce(sum(length(digest)+length(path)+size+128),0) FROM proof_jobs",
        [],
        |row| row.get(0),
    )?;
    usage[0].1 += proofs;
    let mut query = db.prepare("SELECT id,length(intent)+coalesce(length(raw),0)+coalesce(length(error),0)+512 FROM transactions")?;
    for row in query.query_map([], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, u64>(1)?))
    })? {
        let (id, bytes) = row?;
        usage[transaction_class(db, &id)?].1 += bytes;
    }
    let mut query = db.prepare(
        "SELECT budget_group,length(wallet)+length(budget_group)+length(cap)+128 FROM gas_budgets",
    )?;
    for row in query.query_map([], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, u64>(1)?))
    })? {
        let (group, bytes) = row?;
        usage[transaction_class(db, &group)?].1 += bytes;
    }
    Ok(usage)
}
fn check_capacity_limits(db: &Connection, capacity: u64, max_orders: u32) -> Result<()> {
    let usage = capacity_usage(db)?;
    ensure!(
        usage.iter().map(|usage| usage.0).sum::<u32>() <= max_orders
            && usage.iter().map(|usage| usage.1).sum::<u64>() <= capacity,
        "delivery limits cannot discard existing reservations or recovery data"
    );
    for class in [1, 2] {
        ensure!(
            usage[class].0 <= max_orders / 4 && usage[class].1 <= capacity / 4,
            "delivery limits cannot discard existing public commitments"
        );
    }
    check_physical_capacity(db, capacity)
}
fn bind_publication_attempt(
    db: &Connection,
    intent: &SpendIntent,
    (key, body): (&str, &[u8]),
) -> Result<()> {
    ensure!(
        key.starts_with("protected-claim:") && key.len() <= 160 && body.len() <= 1024 * 1024,
        "invalid publication claim"
    );
    let old: Vec<u8> = db
        .query_row(
            "SELECT body FROM private_values WHERE id=?1",
            [key],
            |row| row.get(0),
        )
        .context("publication signing requires its existing durable logical claim")?;
    let mut expected: serde_json::Value = serde_json::from_slice(&old)?;
    let updated: serde_json::Value = serde_json::from_slice(body)?;
    let mut attempts = expected["attempts"]
        .as_array()
        .context("invalid publication attempt ledger")?
        .clone();
    if !attempts.iter().any(|id| id.as_str() == Some(&intent.id)) {
        attempts.push(serde_json::Value::String(intent.id.clone()));
    }
    expected["attempts"] = serde_json::Value::Array(attempts);
    ensure!(
        expected == updated
            && updated["id"]
                .as_str()
                .is_some_and(|id| key == format!("protected-claim:{id}"))
            && updated
                .pointer("/terms/budget_id")
                .and_then(serde_json::Value::as_str)
                .is_some_and(|id| intent.id.starts_with(&format!("budget-{id}/reveal-"))),
        "publication signing changed its logical claim or prior attempts"
    );
    db.execute(
        "UPDATE private_values SET body=?2 WHERE id=?1",
        params![key, body],
    )?;
    Ok(())
}
fn check_physical_capacity(db: &Connection, capacity: u64) -> Result<()> {
    let pages: u64 = db.pragma_query_value(None, "page_count", |row| row.get(0))?;
    let page_size: u64 = db.pragma_query_value(None, "page_size", |row| row.get(0))?;
    let files: u64 = db.query_row("SELECT (SELECT coalesce(sum(size),0) FROM objects)+(SELECT coalesce(sum(size),0) FROM proof_jobs)", [], |row| row.get(0))?;
    ensure!(
        pages
            .checked_mul(page_size)
            .and_then(|bytes| bytes.checked_add(files))
            .is_some_and(|bytes| bytes <= capacity),
        "delivery physical storage quota exceeded"
    );
    Ok(())
}
const MAX_ORDER_BYTES: usize = 64 * 1024;
const HISTORICAL_ORDER: &str = "coalesce(json_extract((SELECT CAST(body AS TEXT) FROM private_values WHERE id='order-observation:'||orders.id),'$[1]'),0)=1
    AND coalesce(json_extract(body,'$.view.payment.settlement.earned')=json_extract(body,'$.view.payment.settlement.withdrawn'),0)
    AND NOT EXISTS(SELECT 1 FROM json_each(body,'$.view.windows') WHERE json_extract(value,'$.state') NOT IN ('earned','refunded'))
    AND json_extract(body,'$.resume_requested')=0 AND json_extract(body,'$.view.error') IS NULL
    AND json_extract(body,'$.view.phase') NOT IN ('reconciling','key_unavailable','failed')
    AND json_extract(body,'$.view.payment.state')!='unknown'
    AND (json_extract(body,'$.view.role')!='receiver'
        OR NOT EXISTS(SELECT 1 FROM json_each(body,'$.view.windows') WHERE json_extract(value,'$.state')!='refunded')
        OR (json_extract(body,'$.view.name') IS NOT NULL AND json_extract(body,'$.view.sha256') IS NOT NULL
            AND json_extract(body,'$.view.evidence') IN ('recoverable','decryption_proven')))";

#[derive(Deserialize)]
struct CapacityOwner {
    view: DeliveryOrder,
    authorized_by: Option<String>,
    confirmed: bool,
    accepted: bool,
    notified: bool,
}
impl CapacityOwner {
    fn class(&self) -> usize {
        match self.view.role {
            Role::Provider => 2,
            Role::Receiver if !self.accepted && !self.confirmed && self.authorized_by.is_none() => {
                1
            }
            _ => 0,
        }
    }
}
fn check_signed_bytes(raw: &str, hash: &str) -> Result<()> {
    ensure!(
        raw.starts_with("0x") && raw.len() <= 512 * 1024 && hash.len() == 66,
        "invalid signed transaction encoding"
    );
    Ok(())
}

#[cfg(test)]
pub(super) mod tests {
    use super::super::types::*;
    use super::*;
    #[allow(dead_code)]
    mod protocol_fixture {
        include!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../crates/delivery/examples/support/mod.rs"
        ));
    }
    pub(in crate::delivery) fn paid_order(role: Role) -> StoredOrder {
        paid_order_at(role, super::super::now().saturating_sub(100))
    }
    fn paid_order_at(role: Role, at: u64) -> StoredOrder {
        let original = protocol_fixture::fixture(&[4, 4], [0x91; 32]);
        let mut terms = original.order.unsigned.terms;
        terms.binding.valid_from = at + 100;
        terms.binding.valid_until = at + 700;
        terms.quote_expires_at = at + 200;
        terms.hold_deadline = at + 300;
        terms.key_deadline = at + 400;
        terms.open_deadline = at + 500;
        terms.retain_until = at + 600;
        terms.priority_deadline = at + 350;
        terms.payer_order_nonce = rand::random();
        let order = protocol_fixture::fixture_from_terms(
            original.profile,
            terms,
            original.plaintexts,
            vibeface_delivery::Amount::from_u64(2),
            vibeface_delivery::Amount::from_u64(1),
        )
        .order;
        let view =
            super::super::orders::view(&uuid::Uuid::new_v4().to_string(), role, &order, 8).unwrap();
        StoredOrder {
            network_id: (role != Role::Provider).then(|| "owner-network".into()),
            authorized_by: None,
            confirmed: false,
            resume_requested: false,
            request_hash: String::new(),
            view,
            order,
            wallet_signature: Vec::new(),
            source_seals: Vec::new(),
            grants: Vec::new(),
            accepted: false,
            notified: false,
            allocation_batch: Vec::new(),
            chain_observed_at_ms: 0,
            settlement_complete: false,
        }
    }
    fn store() -> (ServiceStore, PathBuf) {
        let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../.artifacts/delivery-store-tests")
            .join(uuid::Uuid::new_v4().to_string());
        crate::config::private_dir(&dir).unwrap();
        (ServiceStore::open(&dir, 200_000, 4).unwrap(), dir)
    }
    #[test]
    fn signed_nonce_and_gas_commit_together_and_a_failed_signer_leaves_no_reservation() {
        let (mut store, dir) = store();
        let mut intent =
            super::super::chain::tests::approval_intent(alloy_primitives::Address::from([7; 20]))
                .unwrap();
        intent.id = "provider/hold-0".into();
        let result =
            store.signed_budget_transaction(&intent, 7, "provider", &intent.gas_cap, |_, _| {
                anyhow::bail!("synthetic locked signer")
            });
        assert!(result.is_err());
        assert!(store.find_transaction(&intent.id).unwrap().is_none());
        assert_eq!(
            store
                .db
                .query_row("SELECT count(*) FROM gas_budgets", [], |row| row
                    .get::<_, u32>(0))
                .unwrap(),
            0
        );
        let raw = "0x0201".to_owned();
        let hash = format!("0x{}", "aa".repeat(32));
        let first = store
            .signed_budget_transaction(&intent, 7, "provider", &intent.gas_cap, |_, nonce| {
                assert_eq!(nonce, 7);
                Ok((raw.clone(), hash.clone()))
            })
            .unwrap();
        assert_eq!(first.phase, "signed");
        assert_eq!(first.raw_tx.as_deref(), Some(raw.as_str()));
        drop(store);
        let mut store = ServiceStore::open(&dir, 200_000, 4).unwrap();
        assert_eq!(
            store
                .signed_budget_transaction(&intent, 99, "provider", &intent.gas_cap, |_, _| panic!(
                    "signed retries must reuse exact bytes"
                ))
                .unwrap(),
            first
        );
        let mut changed = intent.clone();
        changed.calldata.push_str("00");
        assert!(
            store
                .signed_budget_transaction(
                    &changed,
                    99,
                    "provider",
                    &intent.gas_cap,
                    |_, _| unreachable!()
                )
                .is_err()
        );
        drop(store);
        std::fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn an_old_unsigned_provider_intent_blocks_sender_and_key_nonces_until_original_recovery() {
        let (mut store, dir) = store();
        let mut intent =
            super::super::chain::tests::approval_intent(alloy_primitives::Address::from([7; 20]))
                .unwrap();
        intent.id = "provider/hold-0".into();
        let original = store
            .reserve_budget_transaction(&intent, 7, "provider", &intent.gas_cap)
            .unwrap();
        for id in ["sender/fund", "key-original/register"] {
            let mut next = intent.clone();
            next.id = id.into();
            assert!(
                store
                    .signed_budget_transaction(
                        &next,
                        100,
                        id.split('/').next().unwrap(),
                        &next.gas_cap,
                        |_, _| panic!("a later intent cannot sign over a hole")
                    )
                    .is_err()
            );
        }
        drop(store);
        let mut store = ServiceStore::open(&dir, 200_000, 4).unwrap();
        let recovered = store
            .signed_budget_transaction(&intent, 100, "provider", &intent.gas_cap, |same, nonce| {
                assert_eq!(same, &original.intent);
                assert_eq!(nonce, original.nonce);
                Ok(("0x0201".into(), format!("0x{}", "aa".repeat(32))))
            })
            .unwrap();
        assert_eq!(recovered.nonce, 7);
        let mut key = intent.clone();
        key.id = "key-original/register".into();
        let key = store
            .signed_budget_transaction(&key, 7, "key-original", &key.gas_cap, |_, _| {
                Ok(("0x0202".into(), format!("0x{}", "bb".repeat(32))))
            })
            .unwrap();
        assert_eq!(key.nonce, 8);
        assert_eq!(store.recent_transactions().unwrap().len(), 2);
        drop(store);
        std::fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn random_public_orders_cannot_take_local_slots_and_failed_admission_is_atomic() {
        let (mut store, dir) = store();
        store.set_limits(2_000_000, 8);
        let providers = (0..2)
            .map(|_| paid_order(Role::Provider))
            .collect::<Vec<_>>();
        for order in &providers {
            store.save_order(order).unwrap();
        }
        let mut incoming = Vec::new();
        for _ in 0..2 {
            let order = paid_order(Role::Receiver);
            store.save_order(&order).unwrap();
            incoming.push(order);
        }
        for role in [Role::Provider, Role::Receiver] {
            let rejected = paid_order(role);
            assert!(store.save_order(&rejected).is_err());
            assert!(store.find_order(&rejected.view.order_id).unwrap().is_none());
            assert_eq!(
                store
                    .db
                    .query_row(
                        "SELECT count(*) FROM objects WHERE order_id=?1",
                        [&rejected.view.order_id],
                        |row| row.get::<_, u32>(0)
                    )
                    .unwrap(),
                0
            );
        }
        let sender = paid_order(Role::Sender);
        store.save_order(&sender).unwrap();
        incoming[0].accepted = true;
        incoming[0].authorized_by = Some("owner".into());
        store.save_order(&incoming[0]).unwrap();
        store.save_order(&paid_order(Role::Receiver)).unwrap();
        let usage = store.usage().unwrap();
        assert_eq!([usage[0].0, usage[1].0, usage[2].0], [2, 2, 2]);
        assert!(usage.iter().map(|entry| entry.1).sum::<u64>() <= store.capacity);
        let files: u64 = store.db.query_row("SELECT (SELECT coalesce(sum(size),0) FROM objects)+(SELECT coalesce(sum(size),0) FROM proof_jobs)", [], |row| row.get(0)).unwrap();
        assert!(
            std::fs::metadata(store.root.join("service.sqlite3"))
                .unwrap()
                .len()
                + files
                <= store.capacity
        );
        assert!(
            providers
                .iter()
                .all(|order| store.find_order(&order.view.order_id).unwrap().is_some())
        );
        drop(store);
        std::fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn receiver_reclamation_keeps_authority_private_files_and_unsigned_ledger_references() {
        let (mut store, dir) = store();
        store.set_limits(2_000_000, 16);
        let expired = paid_order(Role::Receiver);
        store.save_order(&expired).unwrap();
        let mut authorized = paid_order(Role::Receiver);
        authorized.authorized_by = Some("owner".into());
        store.save_order(&authorized).unwrap();
        let referenced = paid_order(Role::Receiver);
        store.save_order(&referenced).unwrap();
        store
            .put_value(
                &format!("charity-file:{}", referenced.view.order_id),
                b"owned file",
            )
            .unwrap();
        let unsigned = paid_order(Role::Receiver);
        store.save_order(&unsigned).unwrap();
        let mut intent =
            super::super::chain::tests::approval_intent(alloy_primitives::Address::from([7; 20]))
                .unwrap();
        intent.id = format!("{}/fund", unsigned.view.order_id);
        store
            .reserve_budget_transaction(&intent, 7, &unsigned.view.order_id, &intent.gas_cap)
            .unwrap();
        store
            .reclaim_unaccepted_receivers(super::super::now() + 1_000, Mode::Paid)
            .unwrap();
        assert!(store.find_order(&expired.view.order_id).unwrap().is_none());
        for order in [&authorized, &referenced, &unsigned] {
            assert!(store.find_order(&order.view.order_id).unwrap().is_some());
        }
        assert!(store.transaction(&intent.id).unwrap().raw_tx.is_none());
        drop(store);
        let store = ServiceStore::open(&dir, 2_000_000, 16).unwrap();
        assert!(store.find_order(&expired.view.order_id).unwrap().is_none());
        assert_eq!(
            store
                .value(&format!("charity-file:{}", referenced.view.order_id))
                .unwrap(),
            Some(b"owned file".to_vec())
        );
        drop(store);
        std::fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn an_expired_unfunded_quote_releases_external_capacity_but_side_effects_never_do() {
        let (mut store, dir) = store();
        store.set_limits(2_000_000, 4);
        let quote = paid_order_at(Role::Provider, super::super::now() - 1_000);
        let body = serde_json::to_value(&quote).unwrap();
        assert!(
            body.get("settlement_complete").is_none() && body.get("chain_observed_at_ms").is_none()
        );
        store.save_order(&quote).unwrap();
        assert!(store.save_order(&paid_order(Role::Provider)).is_err());
        // The caller may use this only after unfunded_after proves the original confirmed chain interval.
        store.reclaim_unfunded_quote(&quote).unwrap();
        assert!(store.find_order(&quote.view.order_id).unwrap().is_none());
        let protected = paid_order_at(Role::Provider, super::super::now() - 1_000);
        store.save_order(&protected).unwrap();
        let mut intent =
            super::super::chain::tests::approval_intent(alloy_primitives::Address::from([7; 20]))
                .unwrap();
        intent.id = format!("{}/publish-0", protected.view.order_id);
        store
            .reserve_budget_transaction(&intent, 7, &protected.view.order_id, &intent.gas_cap)
            .unwrap();
        store.reclaim_unfunded_quote(&protected).unwrap();
        assert!(
            store
                .find_order(&protected.view.order_id)
                .unwrap()
                .is_some()
        );
        assert!(store.transaction(&intent.id).unwrap().raw_tx.is_none());
        drop(store);
        let store = ServiceStore::open(&dir, 2_000_000, 4).unwrap();
        assert!(
            store
                .find_order(&protected.view.order_id)
                .unwrap()
                .is_some()
        );
        drop(store);
        std::fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn historical_orders_do_not_hide_active_work_or_consume_active_slots() {
        let (mut store, dir) = store();
        store.set_limits(4_000_000, 4);
        let active = paid_order(Role::Sender);
        store.save_order(&active).unwrap();
        for index in 0..32 {
            let mut order = paid_order(Role::Sender);
            order.confirmed = true;
            order.settlement_complete = true;
            order.view.phase = Phase::Paid;
            order.view.payment.state = PaymentState::Withdrawn;
            order.view.payment.earned = "7".into();
            order.view.payment.withdrawn = "0".into();
            order.view.payment.settlement = Some(BudgetSettlement {
                earned: "7".into(),
                withdrawn: "7".into(),
                refunded: "0".into(),
                publication: None,
            });
            order.view.updated_at_ms = index;
            for window in &mut order.view.windows {
                window.state = WindowState::Earned;
                window.decryption_proven = true;
            }
            store.save_order(&order).unwrap();
        }
        let work = store.progress_orders(Some("owner-network")).unwrap();
        assert_eq!(work.len(), 2);
        assert_eq!(work.iter().filter(|order| order.historical()).count(), 1);
        assert!(
            work.iter()
                .any(|order| order.view.order_id == active.view.order_id)
        );
        assert_eq!(store.usage().unwrap()[0].0, 1);
        let mut incomplete = paid_order(Role::Sender);
        incomplete.settlement_complete = true;
        incomplete.confirmed = true;
        incomplete.view.phase = Phase::Paid;
        for window in &mut incomplete.view.windows {
            window.state = WindowState::Earned;
        }
        store.save_order(&incomplete).unwrap();
        assert!(!incomplete.historical());
        assert!(
            store
                .progress_orders(Some("owner-network"))
                .unwrap()
                .iter()
                .any(|order| order.view.order_id == incomplete.view.order_id),
            "a missing budget settlement must remain active rather than fall through SQL NULL"
        );
        assert_eq!(store.usage().unwrap()[0].0, 2);
        drop(store);
        std::fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn schema_initializes_once_and_rejects_old_databases_without_changing_data() {
        let (store, dir) = store();
        let version: u32 = store
            .db
            .pragma_query_value(None, "user_version", |r| r.get(0))
            .unwrap();
        assert_eq!(version, crate::config::DATA_EPOCH);
        store
            .db
            .execute(
                "INSERT INTO private_values(id,body) VALUES('retained',x'0102')",
                [],
            )
            .unwrap();
        drop(store);
        let store = ServiceStore::open(&dir, 200_000, 4).unwrap();
        assert_eq!(store.value("retained").unwrap(), Some(vec![1, 2]));
        drop(store);
        let path = dir.join("delivery/service.sqlite3");
        for version in [0, 2, 12] {
            let db = Connection::open(&path).unwrap();
            db.pragma_update(None, "user_version", version).unwrap();
            drop(db);
            let before = std::fs::read(&path).unwrap();
            assert!(ServiceStore::open(&dir, 200_000, 4).is_err());
            assert_eq!(std::fs::read(&path).unwrap(), before);
        }
        std::fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn nonce_is_reserved_before_signing_and_intent_is_immutable() {
        let (mut store, dir) = store();
        let intent = SpendIntent {
            id: "order/fund".into(),
            chain_id: 31337,
            wallet: "wallet".into(),
            to: "escrow".into(),
            calldata: "0x1234".into(),
            value: "0".into(),
            gas_limit: 200000,
            gas_cap: "1000000".into(),
            max_fee_per_gas: "3".into(),
            max_priority_fee_per_gas: "1".into(),
        };
        let first = store.reserve_transaction(&intent, 7).unwrap();
        assert_eq!(first.nonce, 7);
        assert!(first.raw_tx.is_none());
        assert_eq!(store.reserve_transaction(&intent, 9).unwrap().nonce, 7);
        let mut changed = intent.clone();
        changed.calldata = "0x5678".into();
        assert!(store.reserve_transaction(&changed, 7).is_err());
        changed.id = "order/hold".into();
        assert!(store.reserve_transaction(&changed, 7).is_err());
        store
            .sign_transaction(&intent.id, "0x0201", &format!("0x{}", "aa".repeat(32)))
            .unwrap();
        assert_eq!(store.reserve_transaction(&changed, 7).unwrap().nonce, 8);
        drop(store);
        let mut store = ServiceStore::open(&dir, 200_000, 4).unwrap();
        assert_eq!(store.reserve_transaction(&intent, 20).unwrap().nonce, 7);
        drop(store);
        std::fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn provider_reconciliation_keeps_confirmed_transactions_for_reorg_checks() {
        let (mut store, dir) = store();
        let order = "ab".repeat(32);
        let mut intent = SpendIntent {
            id: format!("{order}/hold-0"),
            chain_id: 31337,
            wallet: "wallet".into(),
            to: "escrow".into(),
            calldata: "0x1234".into(),
            value: "0".into(),
            gas_limit: 100000,
            gas_cap: "900000".into(),
            max_fee_per_gas: "3".into(),
            max_priority_fee_per_gas: "1".into(),
        };
        store
            .reserve_budget_transaction(&intent, 7, &order, "900000")
            .unwrap();
        store
            .sign_transaction(&intent.id, "0x0201", &format!("0x{}", "aa".repeat(32)))
            .unwrap();
        let first = intent.id.clone();
        intent.id = format!("{order}/publish-0");
        store
            .reserve_budget_transaction(&intent, 7, &order, "900000")
            .unwrap();
        store
            .sign_transaction(&intent.id, "0x0202", &format!("0x{}", "bb".repeat(32)))
            .unwrap();
        assert_eq!(
            store
                .next_transaction_for_reconcile()
                .unwrap()
                .unwrap()
                .intent
                .id,
            first
        );
        store
            .observe_transaction(&first, "confirmed", Some(8), Some("block-8"), None)
            .unwrap();
        assert_eq!(
            store
                .next_transaction_for_reconcile()
                .unwrap()
                .unwrap()
                .intent
                .id,
            intent.id
        );
        store
            .observe_transaction(&intent.id, "confirmed", Some(9), Some("block-9"), None)
            .unwrap();
        assert!(store.next_transaction_for_reconcile().unwrap().is_none());
        store
            .db
            .execute("UPDATE transactions SET checked_at=0", [])
            .unwrap();
        assert!(store.next_transaction_for_reconcile().unwrap().is_some());
        drop(store);
        std::fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn whole_order_gas_budget_is_atomic_and_counts_every_reserved_action() {
        let (mut store, dir) = store();
        let mut intent = SpendIntent {
            id: "order/fund".into(),
            chain_id: 31337,
            wallet: "wallet".into(),
            to: "escrow".into(),
            calldata: "0x1234".into(),
            value: "0".into(),
            gas_limit: 100000,
            gas_cap: "450000".into(),
            max_fee_per_gas: "3".into(),
            max_priority_fee_per_gas: "1".into(),
        };
        assert_eq!(
            store
                .reserve_budget_transaction(&intent, 7, "order", "450000")
                .unwrap()
                .nonce,
            7
        );
        assert_eq!(
            store
                .reserve_budget_transaction(&intent, 20, "order", "450000")
                .unwrap()
                .nonce,
            7
        );
        store
            .sign_transaction("order/fund", "0x0201", &format!("0x{}", "aa".repeat(32)))
            .unwrap();
        intent.id = "order/hold".into();
        assert!(
            store
                .reserve_budget_transaction(&intent, 8, "order", "450000")
                .is_err()
        );
        assert!(
            store
                .reserve_budget_transaction(&intent, 8, "order", "900000")
                .is_err()
        );
        assert!(store.find_transaction("order/hold").unwrap().is_none());
        assert_eq!(store.recent_transactions().unwrap().len(), 1);
        drop(store);
        std::fs::remove_dir_all(dir).unwrap();
    }

    fn budget_record(order: &StoredOrder, id: &str) -> super::super::budget::BudgetRecord {
        super::super::budget::BudgetRecord {
            terms: super::super::budget::BudgetTerms {
                budget_id: id.into(),
                chain_id: 31337,
                escrow: format!("0x{}", "11".repeat(20)),
                payer: format!("0x{}", "22".repeat(20)),
                payee: format!("0x{}", "33".repeat(20)),
                provider_peer: order.view.provider_peer.clone(),
                asset: format!("0x{}", "44".repeat(20)),
                deposit: "7".into(),
            },
            payer_gas_cap: "600000".into(),
            provider_gas_cap: "600000".into(),
            network_id: order.network_id.clone(),
            authorized_by: order.authorized_by.clone(),
            role: order.view.role,
        }
    }
    fn logical_claim(order: &StoredOrder, budget: &str, id: &str) -> serde_json::Value {
        serde_json::json!({
            "id":id,"terms":{"budget_id":budget,"payee":format!("0x{}","33".repeat(20)),
                "items":[{"window":{"order_id":order.view.order_id,"index":0},"capsule":vec![0;80]}],
                "economic_cutoff":super::super::now()+1000,"total_amount":"7","gas_cap":"600000",
                "policy":{"watchdog_seconds":32,"inclusion_seconds":5,"finality_seconds":5,"clock_skew_seconds":2,
                    "max_fee_per_gas":"3","priority_fee_per_gas":"1"},"expected_earned":{order.view.order_id.clone():"7"}},
            "state":{"state":"private_ready"},"attempts":[],"risk":null,"checked_at_ms":0
        })
    }
    #[test]
    fn protected_budget_capacity_follows_provider_and_receiver_ownership_on_every_surface() {
        for (role, class) in [(Role::Provider, 2), (Role::Receiver, 1)] {
            let (mut store, dir) = store();
            let budget_id = "ab".repeat(32);
            let group = format!("budget-{budget_id}");
            let mut order = paid_order(role);
            order.view.payment.budget_id = Some(budget_id.clone());
            store.save_order(&order).unwrap();
            store
                .put_value(
                    &format!("protected-budget:{budget_id}"),
                    &serde_json::to_vec(&budget_record(&order, &budget_id)).unwrap(),
                )
                .unwrap();
            let hold = format!("protected-hold:{}:0", order.view.order_id);
            store.put_value(&hold, b"original hold").unwrap();
            let claim_id = "cd".repeat(32);
            store
                .put_value(
                    &format!("protected-claim:{claim_id}"),
                    &serde_json::to_vec(&logical_claim(&order, &budget_id, &claim_id)).unwrap(),
                )
                .unwrap();
            store
                .put_value(
                    &format!("protected-claim-window:{}:0", order.view.order_id),
                    claim_id.as_bytes(),
                )
                .unwrap();
            store
                .put_value(
                    &format!("protected-flush:{budget_id}"),
                    &serde_json::to_vec(&vec![super::super::budget::WindowRef {
                        order_id: order.view.order_id.clone(),
                        index: 0,
                    }])
                    .unwrap(),
                )
                .unwrap();
            let mut intent = SpendIntent {
                id: format!("{group}/hold-0"),
                chain_id: 31337,
                wallet: "wallet".into(),
                to: "escrow".into(),
                calldata: "0x1234".into(),
                value: "0".into(),
                gas_limit: 100_000,
                gas_cap: "600000".into(),
                max_fee_per_gas: "3".into(),
                max_priority_fee_per_gas: "1".into(),
            };
            store
                .signed_budget_transaction(&intent, 0, &group, "600000", |_, _| {
                    Ok(("0x0201".into(), format!("0x{}", "aa".repeat(32))))
                })
                .unwrap();
            store
                .put_value(&format!("local-spend:{}", intent.id), b"owner")
                .unwrap();
            let usage = store.usage().unwrap();
            assert_eq!(
                usage[0],
                (0, 0),
                "public budget, hold, claim, claim-window, raw, owner marker and gas ledger cannot become local capacity"
            );
            assert!(usage[class].1 > 0);
            assert_eq!(usage[3 - class], (0, 0));
            assert!(
                store.put_value(&hold, &vec![1; 50_000]).is_err(),
                "public private values share the same quarter cap"
            );
            assert_eq!(store.value(&hold).unwrap().unwrap(), b"original hold");
            intent.id = format!("{group}/hold-1");
            assert!(
                store
                    .signed_budget_transaction(&intent, 1, &group, "600000", |_, _| Ok((
                        format!("0x{}", "01".repeat(30_000)),
                        format!("0x{}", "bb".repeat(32))
                    )))
                    .is_err(),
                "budget signing uses its public quarter cap"
            );
            assert!(store.find_transaction(&intent.id).unwrap().is_none());
            assert_eq!(store.usage().unwrap(), usage);
            if role == Role::Receiver {
                order.accepted = true;
                store.save_order(&order).unwrap();
                let accepted = store.usage().unwrap();
                assert_eq!(accepted[1], (0, 0));
                assert!(
                    accepted[0].1 > 0,
                    "explicit receiver acceptance moves all related budget usage together"
                );
            }
            let missing = format!("budget-{}/hold-0", "ef".repeat(32));
            intent.id = missing.clone();
            assert!(
                store
                    .signed_budget_transaction(
                        &intent,
                        1,
                        missing.split('/').next().unwrap(),
                        "600000",
                        |_, _| panic!("a budget without an owner cannot sign")
                    )
                    .is_err()
            );
            assert!(store.find_transaction(&missing).unwrap().is_none());
            drop(store);
            std::fs::remove_dir_all(dir).unwrap();
        }
    }
    #[test]
    fn an_unfunded_quote_keeps_uncertain_budget_effects_and_preserves_their_ledger_after_cleanup() {
        let (mut store, dir) = store();
        let budget_id = "ab".repeat(32);
        let group = format!("budget-{budget_id}");
        let mut quote = paid_order_at(Role::Provider, super::super::now() - 1_000);
        quote.view.payment.budget_id = Some(budget_id.clone());
        store.save_order(&quote).unwrap();
        store
            .put_value(
                &format!("protected-budget:{budget_id}"),
                &serde_json::to_vec(&budget_record(&quote, &budget_id)).unwrap(),
            )
            .unwrap();
        let intent = SpendIntent {
            id: format!("{group}/withdraw-other-orders"),
            chain_id: 31337,
            wallet: "wallet".into(),
            to: "escrow".into(),
            calldata: "0x1234".into(),
            value: "0".into(),
            gas_limit: 100_000,
            gas_cap: "600000".into(),
            max_fee_per_gas: "3".into(),
            max_priority_fee_per_gas: "1".into(),
        };
        store
            .signed_budget_transaction(&intent, 0, &group, "600000", |_, _| {
                Ok(("0x0201".into(), format!("0x{}", "aa".repeat(32))))
            })
            .unwrap();
        store
            .observe_transaction(&intent.id, "unknown", None, None, None)
            .unwrap();
        store.reclaim_unfunded_quote(&quote).unwrap();
        assert!(
            store.find_order(&quote.view.order_id).unwrap().is_some(),
            "uncertain budget effects retain the quote"
        );
        store
            .observe_transaction(&intent.id, "confirmed", Some(1), Some("canonical"), None)
            .unwrap();
        // Only the production caller's canonical unfunded_after evidence permits this operation.
        store.reclaim_unfunded_quote(&quote).unwrap();
        assert!(
            store.find_order(&quote.view.order_id).unwrap().is_none(),
            "other orders' terminal budget transactions do not pin an unfunded quote forever"
        );
        assert_eq!(
            store.transaction(&intent.id).unwrap().raw_tx.as_deref(),
            Some("0x0201")
        );
        assert!(
            store
                .value(&format!("protected-budget:{budget_id}"))
                .unwrap()
                .is_some()
        );
        assert_eq!(
            store.usage().unwrap()[0],
            (0, 0),
            "retained public ledger still belongs to the public budget"
        );
        drop(store);
        std::fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn provider_retention_keeps_budget_unknowns_claim_candidates_and_dangling_window_references() {
        let (mut store, dir) = store();
        let budget_id = "ab".repeat(32);
        let group = format!("budget-{budget_id}");
        let mut order = paid_order_at(Role::Provider, super::super::now() - 1_000);
        order.accepted = true;
        order.settlement_complete = true;
        order.view.phase = Phase::Paid;
        order.view.payment.state = PaymentState::Withdrawn;
        order.view.payment.budget_id = Some(budget_id.clone());
        order.view.payment.settlement = Some(BudgetSettlement {
            earned: "7".into(),
            withdrawn: "7".into(),
            refunded: "0".into(),
            publication: None,
        });
        for window in &mut order.view.windows {
            window.state = WindowState::Earned;
        }
        store.save_order(&order).unwrap();
        store
            .put_value(
                &format!("protected-budget:{budget_id}"),
                &serde_json::to_vec(&budget_record(&order, &budget_id)).unwrap(),
            )
            .unwrap();
        let intent = SpendIntent {
            id: format!("{group}/reveal-claim-0"),
            chain_id: 31337,
            wallet: "wallet".into(),
            to: "escrow".into(),
            calldata: "0x1234".into(),
            value: "0".into(),
            gas_limit: 100_000,
            gas_cap: "600000".into(),
            max_fee_per_gas: "3".into(),
            max_priority_fee_per_gas: "1".into(),
        };
        store
            .signed_budget_transaction(&intent, 0, &group, "600000", |_, _| {
                Ok(("0x0201".into(), format!("0x{}", "aa".repeat(32))))
            })
            .unwrap();
        store
            .observe_transaction(&intent.id, "unknown", None, None, None)
            .unwrap();
        let remaining = |store: &ServiceStore| {
            store
                .db
                .query_row(
                    "SELECT count(*) FROM objects WHERE order_id=?1 AND kind='ciphertext'",
                    [&order.view.order_id],
                    |row| row.get::<_, u32>(0),
                )
                .unwrap()
        };
        store
            .release_provider_objects(&order.view.order_id)
            .unwrap();
        assert_eq!(
            remaining(&store),
            2,
            "unknown budget raw protects its orders"
        );
        store
            .observe_transaction(&intent.id, "confirmed", Some(1), Some("canonical"), None)
            .unwrap();
        let claim_id = "cd".repeat(32);
        let key = format!("protected-claim:{claim_id}");
        let mut claim = logical_claim(&order, &budget_id, &claim_id);
        claim["attempts"] = serde_json::json!([intent.id]);
        claim["state"] = serde_json::json!({"state":"disclosed_outstanding","first_possible_at":1,"settlement_candidate":null});
        store
            .put_value(&key, &serde_json::to_vec(&claim).unwrap())
            .unwrap();
        store
            .release_provider_objects(&order.view.order_id)
            .unwrap();
        assert_eq!(
            remaining(&store),
            2,
            "a claim awaiting a final candidate retains objects"
        );
        claim["state"] = serde_json::json!({"state":"final_earned","first_possible_at":null});
        store
            .put_value(&key, &serde_json::to_vec(&claim).unwrap())
            .unwrap();
        let dangling = format!("protected-claim-window:{}:1", order.view.order_id);
        store
            .put_value(&dangling, "ef".repeat(32).as_bytes())
            .unwrap();
        store
            .release_provider_objects(&order.view.order_id)
            .unwrap();
        assert_eq!(
            remaining(&store),
            2,
            "missing claim ledger is an unresolved reference"
        );
        store.put_value(&dangling, claim_id.as_bytes()).unwrap();
        store
            .release_provider_objects(&order.view.order_id)
            .unwrap();
        assert_eq!(
            remaining(&store),
            0,
            "confirmed terminal references permit retention cleanup"
        );
        assert_eq!(
            store.transaction(&intent.id).unwrap().raw_tx.as_deref(),
            Some("0x0201")
        );
        assert!(store.value(&key).unwrap().is_some());
        drop(store);
        std::fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn claim_raw_association_is_atomic_and_reverted_attempts_keep_the_original_total_cap() {
        let (mut store, dir) = store();
        let budget_id = "ab".repeat(32);
        let group = format!("budget-{budget_id}");
        let mut order = paid_order(Role::Provider);
        order.view.payment.budget_id = Some(budget_id.clone());
        store.save_order(&order).unwrap();
        store
            .put_value(
                &format!("protected-budget:{budget_id}"),
                &serde_json::to_vec(&budget_record(&order, &budget_id)).unwrap(),
            )
            .unwrap();
        let claim_id = "cd".repeat(32);
        let key = format!("protected-claim:{claim_id}");
        let original = logical_claim(&order, &budget_id, &claim_id);
        let original_bytes = serde_json::to_vec(&original).unwrap();
        store.put_value(&key, &original_bytes).unwrap();
        let mut intent = SpendIntent {
            id: format!("{group}/reveal-claim-0"),
            chain_id: 31337,
            wallet: "wallet".into(),
            to: "escrow".into(),
            calldata: "0x1234".into(),
            value: "0".into(),
            gas_limit: 100_000,
            gas_cap: "600000".into(),
            max_fee_per_gas: "3".into(),
            max_priority_fee_per_gas: "1".into(),
        };
        let mut attempted = original.clone();
        attempted["attempts"] = serde_json::json!([intent.id]);
        let mut body = serde_json::to_vec(&attempted).unwrap();
        assert!(
            store
                .signed_publication_transaction(
                    &intent,
                    0,
                    &group,
                    "600000",
                    (&key, &body),
                    |_, _| anyhow::bail!("locked signer")
                )
                .is_err()
        );
        assert!(store.find_transaction(&intent.id).unwrap().is_none());
        assert_eq!(store.value(&key).unwrap().unwrap(), original_bytes);
        assert_eq!(
            store
                .db
                .query_row("SELECT count(*) FROM gas_budgets", [], |row| row
                    .get::<_, u32>(0))
                .unwrap(),
            0
        );
        assert!(
            store
                .signed_publication_transaction(
                    &intent,
                    0,
                    &group,
                    "600000",
                    (&key, &body),
                    |_, _| Ok((
                        format!("0x{}", "01".repeat(30_000)),
                        format!("0x{}", "aa".repeat(32))
                    ))
                )
                .is_err()
        );
        assert!(store.find_transaction(&intent.id).unwrap().is_none());
        assert_eq!(store.value(&key).unwrap().unwrap(), original_bytes);
        assert_eq!(
            store
                .db
                .query_row("SELECT count(*) FROM gas_budgets", [], |row| row
                    .get::<_, u32>(0))
                .unwrap(),
            0
        );
        store
            .signed_publication_transaction(
                &intent,
                0,
                &group,
                "600000",
                (&key, &body),
                |_, nonce| {
                    assert_eq!(nonce, 0);
                    Ok(("0x0201".into(), format!("0x{}", "aa".repeat(32))))
                },
            )
            .unwrap();
        let first = store.transaction(&intent.id).unwrap();
        store
            .observe_transaction(&intent.id, "unknown", None, None, Some("broadcast unknown"))
            .unwrap();
        intent.id = format!("{group}/reveal-claim-1");
        attempted["attempts"] = serde_json::json!([first.intent.id, intent.id]);
        body = serde_json::to_vec(&attempted).unwrap();
        assert!(
            store
                .signed_publication_transaction(
                    &intent,
                    1,
                    &group,
                    "600000",
                    (&key, &body),
                    |_, _| anyhow::bail!("signer failed")
                )
                .is_err()
        );
        assert!(store.find_transaction(&intent.id).unwrap().is_none());
        assert_eq!(
            store.transaction(&first.intent.id).unwrap().raw_tx,
            first.raw_tx
        );
        assert_eq!(
            store.transaction(&first.intent.id).unwrap().phase,
            "unknown"
        );
        store
            .observe_transaction(
                &first.intent.id,
                "reverted",
                Some(1),
                Some("canonical-block"),
                None,
            )
            .unwrap();
        assert_eq!(
            store
                .signed_publication_transaction(
                    &intent,
                    1,
                    &group,
                    "600000",
                    (&key, &body),
                    |_, _| Ok(("0x0202".into(), format!("0x{}", "bb".repeat(32))))
                )
                .unwrap()
                .nonce,
            1
        );
        intent.id = format!("{group}/reveal-claim-2");
        let mut capped = attempted.clone();
        capped["attempts"]
            .as_array_mut()
            .unwrap()
            .push(serde_json::json!(intent.id));
        assert!(
            store
                .signed_publication_transaction(
                    &intent,
                    2,
                    &group,
                    "600000",
                    (&key, &serde_json::to_vec(&capped).unwrap()),
                    |_, _| panic!("original cumulative cap blocks before signing")
                )
                .is_err()
        );
        assert!(store.find_transaction(&intent.id).unwrap().is_none());
        assert_eq!(store.value(&key).unwrap().unwrap(), body);
        drop(store);
        let store = ServiceStore::open(&dir, 200_000, 4).unwrap();
        assert_eq!(store.value(&key).unwrap().unwrap(), body);
        assert_eq!(
            store
                .transaction(&first.intent.id)
                .unwrap()
                .raw_tx
                .as_deref(),
            Some("0x0201")
        );
        assert!(
            store.next_transaction_for_reconcile().unwrap().is_none(),
            "generic reconciliation must never broadcast an E-bearing claim attempt"
        );
        drop(store);
        std::fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn object_restarts_at_durable_prefix_and_rejects_escape_and_bad_hash() {
        let (mut store, dir) = store();
        let order = "ab".repeat(32);
        let bytes = vec![0x42; 10_000];
        let digest = hex::encode(hash(&bytes));
        assert_eq!(
            store
                .reserve_object(&order, 0, "ciphertext", &digest, 10000)
                .unwrap(),
            0
        );
        assert_eq!(
            store
                .append_object(&order, 0, "ciphertext", 0, &bytes[..8192])
                .unwrap(),
            8192
        );
        drop(store);
        let mut store = ServiceStore::open(&dir, 200_000, 4).unwrap();
        assert_eq!(
            store
                .reserve_object(&order, 0, "ciphertext", &digest, 10000)
                .unwrap(),
            8192
        );
        assert!(
            store
                .append_object(&order, 0, "ciphertext", 0, b"x")
                .is_err()
        );
        store
            .append_object(&order, 0, "ciphertext", 8192, &bytes[8192..])
            .unwrap();
        assert_eq!(store.object(&order, 0, "ciphertext").unwrap(), bytes);
        assert!(
            store
                .reserve_object("../escape", 0, "ciphertext", &digest, 2)
                .is_err()
        );
        assert!(
            store
                .reserve_object(&order, 1, "other", &digest, 2)
                .is_err()
        );
        assert!(
            store
                .reserve_object(&order, 1, "capsule", &digest, 81)
                .is_err()
        );
        drop(store);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
