use super::types::{StoredOrder, parse_word};
use alloy_primitives::U256;
use anyhow::{Context, Result, ensure};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use std::{
    fs::{File, OpenOptions},
    io::{Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
};
use vibeface_delivery::hash;

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
    use std::os::unix::fs::OpenOptionsExt;
    let mut options = OpenOptions::new();
    options
        .read(true)
        .write(write)
        .create(create)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW);
    let file = options.open(path)?;
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
        let db = crate::store::private_file_connection(&root.join("service.sqlite3"))?;
        db.busy_timeout(std::time::Duration::from_secs(5))?;
        db.execute_batch("PRAGMA journal_mode=DELETE; PRAGMA synchronous=FULL;
            CREATE TABLE IF NOT EXISTS orders(id TEXT PRIMARY KEY,network TEXT,intent TEXT NOT NULL,role TEXT NOT NULL,body TEXT NOT NULL);
            CREATE INDEX IF NOT EXISTS orders_intent ON orders(network,intent,role);
            CREATE TABLE IF NOT EXISTS charity_orders(id TEXT PRIMARY KEY,network TEXT,intent TEXT NOT NULL,body TEXT NOT NULL);
            CREATE INDEX IF NOT EXISTS charity_intent ON charity_orders(network,intent);
            CREATE TABLE IF NOT EXISTS objects(order_id TEXT NOT NULL,idx INTEGER NOT NULL,kind TEXT NOT NULL,hash TEXT NOT NULL,size INTEGER NOT NULL,committed INTEGER NOT NULL DEFAULT 0,complete INTEGER NOT NULL DEFAULT 0,PRIMARY KEY(order_id,idx,kind));
            CREATE TABLE IF NOT EXISTS private_values(id TEXT PRIMARY KEY,body BLOB NOT NULL);
            CREATE TABLE IF NOT EXISTS proof_jobs(digest TEXT PRIMARY KEY,path TEXT NOT NULL,size INTEGER NOT NULL);
            CREATE TABLE IF NOT EXISTS gas_budgets(chain INTEGER NOT NULL,wallet TEXT NOT NULL,budget_group TEXT NOT NULL,cap TEXT NOT NULL,PRIMARY KEY(chain,wallet,budget_group));
            CREATE TABLE IF NOT EXISTS transactions(id TEXT PRIMARY KEY,chain INTEGER NOT NULL,wallet TEXT NOT NULL,nonce INTEGER NOT NULL,intent TEXT NOT NULL,raw TEXT,hash TEXT,phase TEXT NOT NULL,block_number INTEGER,block_hash TEXT,error TEXT,checked_at INTEGER NOT NULL DEFAULT 0,UNIQUE(chain,wallet,nonce));")?;
        Ok(Self {
            db,
            root,
            capacity,
            max_orders,
        })
    }
    pub fn root(&self) -> &Path {
        &self.root
    }
    pub fn check_limits(&self, capacity: u64, max_orders: u32) -> Result<()> {
        let used:u64=self.db.query_row("SELECT (SELECT coalesce(sum(size),0) FROM objects)+(SELECT coalesce(sum(length(body)),0) FROM private_values)+(SELECT coalesce(sum(size),0) FROM proof_jobs)",[],|r|r.get(0))?;
        let count: u32 = self.db.query_row(
            "SELECT (SELECT count(*) FROM orders)+(SELECT count(*) FROM charity_orders)",
            [],
            |r| r.get(0),
        )?;
        ensure!(
            used <= capacity && count <= max_orders,
            "delivery limits cannot discard existing reservations or recovery data"
        );
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
        let used:u64=self.db.query_row("SELECT (SELECT coalesce(sum(size),0) FROM objects)+(SELECT coalesce(sum(length(body)),0) FROM private_values)+(SELECT coalesce(sum(size),0) FROM proof_jobs)",[],|r|r.get(0))?;
        ensure!(
            used.checked_add(16 * 1024)
                .is_some_and(|used| used <= self.capacity),
            "proof receipt storage quota exceeded"
        );
        self.db.execute(
            "INSERT INTO proof_jobs(digest,path,size) VALUES(?1,?2,16384)",
            params![digest, path.to_str().context("invalid receipt path")?],
        )?;
        Ok(path)
    }
    pub fn order(&self, id: &str) -> Result<StoredOrder> {
        parse_word(id)?;
        let body: String = self
            .db
            .query_row("SELECT body FROM orders WHERE id=?1", [id], |r| r.get(0))
            .context("delivery order not found")?;
        Ok(serde_json::from_str(&body)?)
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
        let role = serde_json::to_string(&order.view.role)?;
        let existing: Option<(Option<String>, String)> = self
            .db
            .query_row(
                "SELECT network,role FROM orders WHERE id=?1",
                [&order.view.order_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        if let Some((network, existing_role)) = existing {
            ensure!(
                network == order.network_id && role == existing_role,
                "delivery order owner cannot change"
            );
        } else {
            let count: u32 = self.db.query_row(
                "SELECT (SELECT count(*) FROM orders)+(SELECT count(*) FROM charity_orders)",
                [],
                |r| r.get(0),
            )?;
            ensure!(count < self.max_orders, "delivery order capacity exhausted");
        }
        self.db.execute("INSERT INTO orders(id,network,intent,role,body) VALUES(?1,?2,?3,?4,?5) ON CONFLICT(id) DO UPDATE SET body=excluded.body",
            params![order.view.order_id,order.network_id,order.view.intent_id,role,serde_json::to_string(order)?])?;
        Ok(())
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
        body.map(|body| serde_json::from_str(&body).map_err(Into::into))
            .transpose()
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
            .map(|body| serde_json::from_str(&body).map_err(Into::into))
            .collect()
    }
    pub fn save_charity_order(
        &mut self,
        id: &str,
        network: Option<&str>,
        intent: &str,
        body: &str,
    ) -> Result<()> {
        parse_word(id)?;
        ensure!(
            body.len() <= MAX_CHARITY_ORDER_BYTES,
            "charity order exceeds its control bound"
        );
        let prior: Option<Option<String>> = self
            .db
            .query_row(
                "SELECT network FROM charity_orders WHERE id=?1",
                [id],
                |r| r.get(0),
            )
            .optional()?;
        if let Some(prior) = prior {
            ensure!(
                prior.as_deref() == network,
                "charity order owner cannot change"
            );
        } else {
            let count: u32 = self.db.query_row(
                "SELECT (SELECT count(*) FROM charity_orders)+(SELECT count(*) FROM orders)",
                [],
                |r| r.get(0),
            )?;
            ensure!(count < self.max_orders, "delivery order capacity exhausted");
        }
        self.db.execute("INSERT INTO charity_orders(id,network,intent,body) VALUES(?1,?2,?3,?4) ON CONFLICT(id) DO UPDATE SET body=excluded.body",params![id,network,intent,body])?;
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
        ensure!(
            id.len() <= 160 && value.len() <= 1024 * 1024,
            "private delivery value too large"
        );
        let used: u64 = self.db.query_row(
            "SELECT (SELECT coalesce(sum(length(body)),0) FROM private_values WHERE id<>?1)+(SELECT coalesce(sum(size),0) FROM objects)+(SELECT coalesce(sum(size),0) FROM proof_jobs)",
            [id],
            |r| r.get(0),
        )?;
        ensure!(
            used.checked_add(value.len() as u64)
                .is_some_and(|n| n <= self.capacity),
            "private delivery storage quota exceeded"
        );
        self.db.execute("INSERT INTO private_values(id,body) VALUES(?1,?2) ON CONFLICT(id) DO UPDATE SET body=excluded.body",params![id,value])?;
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
    fn object_path(&self, order: &str, index: u16, kind: &str, partial: bool) -> Result<PathBuf> {
        parse_word(order)?;
        ensure!(
            index < 16 && matches!(kind, "ciphertext" | "capsule"),
            "invalid delivery object path"
        );
        let parent = self.root.join("objects").join(order);
        private_directory(&parent)?;
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
        parse_word(digest)?;
        let path = self.object_path(order, index, kind, false)?;
        ensure!(
            size > 0 && size <= if kind == "capsule" { 80 } else { 65_604 },
            "delivery object exceeds window limit"
        );
        let tx = self
            .db
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let prior: Option<(String, u32, u32)> = tx
            .query_row(
                "SELECT hash,size,committed FROM objects WHERE order_id=?1 AND idx=?2 AND kind=?3",
                params![order, index, kind],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?;
        let committed = if let Some((old_hash, old_size, committed)) = prior {
            ensure!(
                old_hash == digest && old_size == size,
                "delivery object changed across retry"
            );
            committed
        } else {
            let used: u64 = tx.query_row("SELECT (SELECT coalesce(sum(size),0) FROM objects)+(SELECT coalesce(sum(length(body)),0) FROM private_values)+(SELECT coalesce(sum(size),0) FROM proof_jobs)", [], |r| {
                r.get(0)
            })?;
            ensure!(
                used.checked_add(u64::from(size))
                    .is_some_and(|n| n <= self.capacity),
                "delivery storage quota exceeded"
            );
            tx.execute(
                "INSERT INTO objects(order_id,idx,kind,hash,size) VALUES(?1,?2,?3,?4,?5)",
                params![order, index, kind, digest, size],
            )?;
            0
        };
        tx.commit()?;
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
            File::open(partial.parent().context("object directory missing")?)?.sync_all()?;
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
    pub fn reserve_transaction(
        &mut self,
        intent: &SpendIntent,
        pending_nonce: u64,
    ) -> Result<SignedTransaction> {
        self.reserve_transaction_inner(intent, pending_nonce, None)
    }
    pub fn reserve_budget_transaction(
        &mut self,
        intent: &SpendIntent,
        pending_nonce: u64,
        budget_group: &str,
        total_gas_cap: &str,
    ) -> Result<SignedTransaction> {
        self.reserve_transaction_inner(intent, pending_nonce, Some((budget_group, total_gas_cap)))
    }
    fn reserve_transaction_inner(
        &mut self,
        intent: &SpendIntent,
        pending_nonce: u64,
        budget: Option<(&str, &str)>,
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
        let prior:Option<TxRow>=tx.query_row("SELECT intent,nonce,raw,hash,phase,block_number,block_hash,error FROM transactions WHERE id=?1",[&intent.id],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?,r.get(5)?,r.get(6)?,r.get(7)?))).optional()?;
        if let Some((old, nonce, raw_tx, tx_hash, phase, block_number, block_hash, error)) = prior {
            ensure!(old == encoded, "transaction intent changed across retry");
            return Ok(SignedTransaction {
                intent: intent.clone(),
                nonce,
                raw_tx,
                tx_hash,
                phase,
                block_number,
                block_hash,
                error,
            });
        }
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
        tx.execute("INSERT INTO transactions(id,chain,wallet,nonce,intent,phase) VALUES(?1,?2,?3,?4,?5,'reserved')",params![intent.id,intent.chain_id,intent.wallet,nonce,encoded])?;
        tx.commit()?;
        Ok(SignedTransaction {
            intent: intent.clone(),
            nonce,
            raw_tx: None,
            tx_hash: None,
            phase: "reserved".into(),
            block_number: None,
            block_hash: None,
            error: None,
        })
    }
    pub fn sign_transaction(&self, id: &str, raw: &str, hash: &str) -> Result<()> {
        let prior = self.transaction(id)?;
        ensure!(
            prior.raw_tx.as_deref().is_none_or(|v| v == raw)
                && prior.tx_hash.as_deref().is_none_or(|v| v == hash),
            "reserved transaction was already signed differently"
        );
        ensure!(
            raw.starts_with("0x") && raw.len() <= 512 * 1024 && hash.len() == 66,
            "invalid signed transaction encoding"
        );
        self.db.execute("UPDATE transactions SET raw=?2,hash=?3,phase=CASE WHEN raw IS NULL THEN 'signed' ELSE phase END WHERE id=?1",params![id,raw,hash])?;
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
        let id:Option<String>=self.db.query_row("SELECT id FROM transactions WHERE raw IS NOT NULL AND phase NOT IN ('failed','reverted','conflicted') ORDER BY checked_at,nonce LIMIT 1",[],|r|r.get(0)).optional()?;
        id.map(|id| self.transaction(&id)).transpose()
    }
    pub fn recent_transactions(&self) -> Result<Vec<SignedTransaction>> {
        let mut query = self
            .db
            .prepare("SELECT id FROM transactions ORDER BY rowid DESC LIMIT 128")?;
        let ids = query
            .query_map([], |r| r.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        ids.iter().map(|id| self.transaction(id)).collect()
    }
    pub fn pending_transactions(&self) -> Result<Vec<SignedTransaction>> {
        let mut query=self.db.prepare("SELECT id FROM transactions WHERE phase NOT IN ('confirmed','failed','conflicted') ORDER BY chain,wallet,nonce LIMIT 128")?;
        let ids = query
            .query_map([], |r| r.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        ids.iter().map(|id| self.transaction(id)).collect()
    }
}
const MAX_CHARITY_ORDER_BYTES: usize = 16 * 1024;

#[cfg(test)]
mod tests {
    use super::*;
    fn store() -> (ServiceStore, PathBuf) {
        let dir = std::env::temp_dir().join(format!("vdp-store-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&dir).unwrap();
        (ServiceStore::open(&dir, 200_000, 4).unwrap(), dir)
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
        assert_eq!(store.reserve_transaction(&changed, 7).unwrap().nonce, 8);
        drop(store);
        let mut store = ServiceStore::open(&dir, 200_000, 4).unwrap();
        assert_eq!(store.reserve_transaction(&intent, 20).unwrap().nonce, 7);
        std::fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn provider_reconciliation_keeps_confirmed_transactions_for_reorg_checks() {
        let (mut store, dir) = store();
        let order = "ab".repeat(32);
        store.db.execute("INSERT INTO orders(id,network,intent,role,body) VALUES(?1,NULL,'provider-intent','\"provider\"','{}')",[&order]).unwrap();
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
        std::fs::remove_dir_all(dir).unwrap();
    }
}
