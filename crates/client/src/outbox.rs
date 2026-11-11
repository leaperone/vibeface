//! Private durable operations. A committed unknown outcome blocks its original write lane.
use crate::wire::Attachment;
use anyhow::{Context, Result, ensure};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use vibeface_bridge::platform::privatefs;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OperationState {
    Prepared,
    Unknown,
    Accepted,
    Rejected,
    ClosedByUser,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OperationSnapshot {
    pub id: String,
    pub network_id: String,
    pub client_id: String,
    pub target_node: String,
    pub state: OperationState,
    pub sequence: Option<u64>,
    pub result: Option<Value>,
}
#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct Operation {
    pub snapshot: OperationSnapshot,
    pub lane: String,
    pub expected_revision: Option<u64>,
    pub bytes: Vec<u8>,
    pub reconciliation: Reconciliation,
    pub attachments: Vec<Attachment>,
    pub upload_scope: Option<String>,
    pub prerequisites: Vec<String>,
}
#[derive(Clone, Serialize, Deserialize)]
pub(crate) enum Reconciliation {
    Input {
        conversation_id: String,
        message_id: String,
        target: crate::wire::InputTarget,
    },
    Memory {
        conversation_id: String,
        memory_id: String,
    },
    /// The server currently exposes no authoritative receipt lookup for this command.
    Workspace,
    Signed {
        scope_id: String,
        body: Value,
        attestation: Value,
    },
    Delivery {
        intent_id: Option<String>,
        order_id: Option<String>,
        operation: String,
    },
    StopReply {
        conversation_id: String,
        trigger: String,
    },
    StopWork {
        conversation_id: String,
        work_id: String,
    },
    Settings {
        kind: crate::SettingsKind,
    },
    Manual,
}

pub struct Outbox {
    _lock: std::fs::File,
    db: Connection,
    dir: PathBuf,
    network: String,
    client: String,
}
impl Outbox {
    pub(crate) fn stored_network(dir: &Path, client: &str) -> Result<String> {
        privatefs::ensure_private(dir, "client outbox directory")?;
        let path = dir.join("outbox.sqlite3");
        privatefs::ensure_private(&path, "client outbox")?;
        drop(privatefs::open_no_follow(&path)?);
        let db = Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        let (network, owner): (String, String) =
            db.query_row("SELECT network,client FROM identity", [], |row| {
                Ok((row.get(0)?, row.get(1)?))
            })?;
        ensure!(
            owner == client,
            "private pending data belongs to another client"
        );
        vibeface_secure::membership::validate_hash(&network)?;
        Ok(network)
    }
    pub fn open(dir: &Path, network: &str, client: &str) -> Result<Self> {
        privatefs::create_dir(dir)?;
        privatefs::ensure_private(dir, "client outbox")?;
        privatefs::create_dir(&dir.join("attachments"))?;
        let lock = privatefs::open_or_create(&dir.join("outbox.lock"))?;
        lock.try_lock()
            .context("outbox is already open in another client")?;
        let path = dir.join("outbox.sqlite3");
        if !path.exists() {
            privatefs::create_durable(&path, &[])?;
        }
        privatefs::ensure_private(&path, "client outbox database")?;
        // Open the checked file without following symlinks, before SQLite opens it.
        drop(privatefs::open_no_follow(&path)?);
        let db = Connection::open(&path)?;
        db.busy_timeout(std::time::Duration::from_secs(3))?;
        db.execute_batch("PRAGMA journal_mode=DELETE; PRAGMA synchronous=FULL;
          CREATE TABLE IF NOT EXISTS identity(network TEXT NOT NULL, client TEXT NOT NULL);
          CREATE TABLE IF NOT EXISTS operations(id TEXT PRIMARY KEY, lane TEXT NOT NULL, state TEXT NOT NULL, payload BLOB NOT NULL);
          CREATE INDEX IF NOT EXISTS operations_lane ON operations(lane, state);
          CREATE TABLE IF NOT EXISTS lane_sequences(lane TEXT PRIMARY KEY, sequence INTEGER NOT NULL);
          CREATE TABLE IF NOT EXISTS reserved_sequences(lane TEXT PRIMARY KEY, sequence INTEGER NOT NULL);
          CREATE TABLE IF NOT EXISTS attachment_leases(owner TEXT NOT NULL,hash TEXT NOT NULL,PRIMARY KEY(owner,hash));
          CREATE TABLE IF NOT EXISTS verified_profiles(target TEXT PRIMARY KEY,payload BLOB NOT NULL);")?;
        let identity: Option<(String, String)> = db
            .query_row("SELECT network,client FROM identity", [], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .optional()?;
        match identity {
            Some((n, c)) => ensure!(
                n == network && c == client,
                "outbox belongs to another network or client"
            ),
            None => {
                db.execute(
                    "INSERT INTO identity VALUES(?1,?2)",
                    params![network, client],
                )?;
            }
        }
        privatefs::sync_dir(dir)?;
        let outbox = Self {
            _lock: lock,
            db,
            dir: dir.to_owned(),
            network: network.into(),
            client: client.into(),
        };
        Ok(outbox)
    }
    pub(crate) fn cache_profile(&self, profile: &crate::profile::CachedProfile) -> Result<()> {
        ensure!(
            profile.network_id == self.network && profile.client_id == self.client,
            "profile cache identity mismatch"
        );
        let existing: Option<Vec<u8>> = self
            .db
            .query_row(
                "SELECT payload FROM verified_profiles WHERE target=?1",
                [&profile.target_node],
                |row| row.get(0),
            )
            .optional()?;
        if let Some(existing) = existing {
            let known: crate::profile::CachedProfile = serde_json::from_slice(&existing)?;
            if known.revoked && !profile.revoked {
                let prior = known.state.project_at(known.observed_at_ms)?;
                let fresh = profile.state.project_at(profile.observed_at_ms)?;
                ensure!(
                    fresh.epoch > prior.epoch,
                    "known revocation needs an explicit newer root recovery"
                );
                fresh.client(&profile.client_id)?;
                fresh.node(&profile.target_node)?;
            }
            ensure!(
                known.write_gate != 2 || profile.write_gate == 2,
                "known Node fork cannot be overwritten by older write status"
            );
        }
        let bytes = serde_json::to_vec(profile)?;
        ensure!(
            bytes.len() <= 4 * 1024 * 1024,
            "profile proof exceeds cache limit"
        );
        let count: u64 =
            self.db
                .query_row("SELECT count(*) FROM verified_profiles", [], |row| {
                    row.get(0)
                })?;
        let exists: bool = self.db.query_row(
            "SELECT EXISTS(SELECT 1 FROM verified_profiles WHERE target=?1)",
            [&profile.target_node],
            |row| row.get(0),
        )?;
        ensure!(count < 64 || exists, "too many cached Node profiles");
        self.db.execute("INSERT INTO verified_profiles VALUES(?1,?2) ON CONFLICT(target) DO UPDATE SET payload=excluded.payload",params![profile.target_node,bytes])?;
        Ok(())
    }
    pub(crate) fn cached_profile(&self, target: &str) -> Result<crate::profile::CachedProfile> {
        let bytes: Vec<u8> = self.db.query_row(
            "SELECT payload FROM verified_profiles WHERE target=?1",
            [target],
            |row| row.get(0),
        )?;
        ensure!(
            bytes.len() <= 4 * 1024 * 1024,
            "profile cache exceeds limit"
        );
        Ok(serde_json::from_slice(&bytes)?)
    }
    pub(crate) fn cache_write_gate(&self, target: &str, gate: u8) -> Result<()> {
        let mut profile = self.cached_profile(target)?;
        if profile.write_gate != 2 {
            profile.write_gate = gate;
        }
        self.cache_profile(&profile)
    }
    pub(crate) fn invalidate_profile(&self, target: &str) -> Result<()> {
        let mut profile = match self.cached_profile(target) {
            Ok(profile) => profile,
            Err(error)
                if error
                    .downcast_ref::<rusqlite::Error>()
                    .is_some_and(|error| matches!(error, rusqlite::Error::QueryReturnedNoRows)) =>
            {
                return Ok(());
            }
            Err(error) => return Err(error),
        };
        profile.revoked = true;
        self.cache_profile(&profile)
    }
    pub(crate) fn reserve_sequence(&mut self, lane: &str) -> Result<u64> {
        let tx = self
            .db
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let previous:u64=tx.query_row("SELECT MAX(sequence) FROM (SELECT sequence FROM lane_sequences WHERE lane=?1 UNION ALL SELECT sequence FROM reserved_sequences WHERE lane=?1 UNION ALL SELECT 0)",[lane],|r|r.get(0))?;
        let next = previous
            .checked_add(1)
            .context("input sequence exhausted")?;
        ensure!(next <= 9_007_199_254_740_991, "input sequence exhausted");
        tx.execute("INSERT INTO reserved_sequences VALUES(?1,?2) ON CONFLICT(lane) DO UPDATE SET sequence=excluded.sequence",params![lane,next])?;
        tx.commit()?;
        Ok(next)
    }
    pub fn snapshots(&self) -> Result<Vec<OperationSnapshot>> {
        Ok(self.operations()?.into_iter().map(|o| o.snapshot).collect())
    }
    fn operations(&self) -> Result<Vec<Operation>> {
        let mut query = self
            .db
            .prepare("SELECT payload FROM operations ORDER BY rowid")?;
        let rows = query.query_map([], |r| r.get::<_, Vec<u8>>(0))?;
        rows.map(|r| Ok(serde_json::from_slice(&r?)?)).collect()
    }
    pub(crate) fn find(&self, id: &str) -> Result<Option<Operation>> {
        let bytes: Option<Vec<u8>> = self
            .db
            .query_row("SELECT payload FROM operations WHERE id=?1", [id], |row| {
                row.get(0)
            })
            .optional()?;
        bytes
            .map(|bytes| Ok(serde_json::from_slice(&bytes)?))
            .transpose()
    }
    pub(crate) fn can_begin(&self, id: &str) -> Result<bool> {
        let operation = self.get(id)?;
        if operation.snapshot.state != OperationState::Prepared {
            return Ok(false);
        }
        let blocked:bool=self.db.query_row("SELECT EXISTS(SELECT 1 FROM operations WHERE lane=?1 AND state IN ('prepared','unknown') AND rowid<(SELECT rowid FROM operations WHERE id=?2))",params![operation.lane,id],|row|row.get(0))?;
        if blocked {
            return Ok(false);
        }
        for id in &operation.prerequisites {
            if self.get(id)?.snapshot.state != OperationState::Accepted {
                return Ok(false);
            }
        }
        Ok(true)
    }
    pub(crate) fn get(&self, id: &str) -> Result<Operation> {
        let bytes: Vec<u8> =
            self.db
                .query_row("SELECT payload FROM operations WHERE id=?1", [id], |r| {
                    r.get(0)
                })?;
        Ok(serde_json::from_slice(&bytes)?)
    }
    pub(crate) fn prepare(&mut self, operation: &Operation) -> Result<()> {
        ensure!(
            operation.snapshot.network_id == self.network
                && operation.snapshot.client_id == self.client,
            "operation identity mismatch"
        );
        ensure!(
            operation.bytes.len() <= 1024 * 1024,
            "operation exceeds limit"
        );
        for attachment in &operation.attachments {
            self.read_attachment(attachment)?;
        }
        let tx = self
            .db
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let existing: Option<Vec<u8>> = tx
            .query_row(
                "SELECT payload FROM operations WHERE id=?1",
                [&operation.snapshot.id],
                |r| r.get(0),
            )
            .optional()?;
        if let Some(bytes) = existing {
            let old: Operation = serde_json::from_slice(&bytes)?;
            ensure!(
                old.bytes == operation.bytes
                    && old.snapshot.target_node == operation.snapshot.target_node,
                "business ID already has a different signed operation"
            );
            return Ok(());
        }
        tx.execute("DELETE FROM operations WHERE state IN ('accepted','closed_by_user') AND rowid NOT IN (SELECT rowid FROM operations WHERE state IN ('accepted','closed_by_user') ORDER BY rowid DESC LIMIT 64) AND NOT EXISTS(SELECT 1 FROM operations pending,json_each(CAST(pending.payload AS TEXT),'$.prerequisites') dependency WHERE pending.state!='accepted' AND dependency.value=operations.id)", [])?;
        let count: u64 = tx.query_row("SELECT count(*) FROM operations", [], |r| r.get(0))?;
        ensure!(
            count < 256,
            "outbox is full; reconcile outstanding operations"
        );
        ensure!(
            operation.prerequisites.len() <= 16,
            "too many operation prerequisites"
        );
        for prerequisite in &operation.prerequisites {
            let bytes: Vec<u8> = tx.query_row(
                "SELECT payload FROM operations WHERE id=?1",
                [prerequisite],
                |row| row.get(0),
            )?;
            let prior: Operation = serde_json::from_slice(&bytes)?;
            ensure!(
                prior.snapshot.target_node == operation.snapshot.target_node,
                "prerequisite belongs to another target"
            );
        }
        if let Some(sequence) = operation.snapshot.sequence {
            ensure!(
                sequence <= 9_007_199_254_740_991,
                "sequence exceeds wire integer range"
            );
            let last: Option<u64> = tx
                .query_row(
                    "SELECT sequence FROM lane_sequences WHERE lane=?1",
                    [&operation.lane],
                    |r| r.get(0),
                )
                .optional()?;
            ensure!(
                last.is_none_or(|last| sequence > last),
                "input sequence must advance monotonically"
            );
            tx.execute("INSERT INTO lane_sequences VALUES(?1,?2) ON CONFLICT(lane) DO UPDATE SET sequence=excluded.sequence",params![operation.lane,sequence])?;
        }
        tx.execute(
            "INSERT INTO operations VALUES(?1,?2,'prepared',?3)",
            params![
                operation.snapshot.id,
                operation.lane,
                serde_json::to_vec(operation)?
            ],
        )?;
        tx.commit()?;
        Ok(())
    }
    pub(crate) fn dismiss_rejected(&mut self, id: &str) -> Result<()> {
        let operation = self.get(id)?;
        ensure!(
            operation.snapshot.state == OperationState::Rejected,
            "only an explicitly rejected operation may be discarded"
        );
        self.db.execute(
            "DELETE FROM operations WHERE id=?1 AND state='rejected'",
            [id],
        )?;
        Ok(())
    }
    /// Persists uncertainty BEFORE crossing the network boundary. Process death cannot create a retryable write.
    pub(crate) fn begin(&mut self, id: &str) -> Result<Operation> {
        ensure!(
            self.can_begin(id)?,
            "an earlier operation or prerequisite still needs confirmation"
        );
        let mut op = self.get(id)?;
        ensure!(
            op.snapshot.state == OperationState::Prepared,
            "operation must be reconciled before retry"
        );
        op.snapshot.state = OperationState::Unknown;
        self.save(&op, "unknown")?;
        Ok(op)
    }
    fn affected_prepared(&self, id: &str) -> Result<Vec<Operation>> {
        let all = self.operations()?;
        let root = all
            .iter()
            .position(|op| op.snapshot.id == id)
            .context("operation missing")?;
        let mut affected = std::collections::HashSet::from([id.to_owned()]);
        let mut lanes = std::collections::HashMap::from([(all[root].lane.clone(), root)]);
        let mut changed = true;
        while changed {
            changed = false;
            for (index, op) in all.iter().enumerate() {
                if op.snapshot.state != OperationState::Prepared
                    || affected.contains(&op.snapshot.id)
                {
                    continue;
                }
                if op
                    .prerequisites
                    .iter()
                    .any(|dependency| affected.contains(dependency))
                    || lanes.get(&op.lane).is_some_and(|prior| index > *prior)
                {
                    affected.insert(op.snapshot.id.clone());
                    lanes.entry(op.lane.clone()).or_insert(index);
                    changed = true;
                }
            }
        }
        Ok(all
            .into_iter()
            .filter(|op| affected.contains(&op.snapshot.id))
            .collect())
    }
    pub(crate) fn review_settings(&self, id: &str) -> Result<crate::SettingsReview> {
        let operation = self.get(id)?;
        let dependent_ids = self
            .affected_prepared(id)?
            .into_iter()
            .filter(|op| op.snapshot.id != id)
            .map(|op| op.snapshot.id)
            .collect();
        crate::settings::review(&operation, dependent_ids)
    }
    pub(crate) fn close_unknown_settings(
        &mut self,
        review: &crate::SettingsReview,
        note: &str,
    ) -> Result<Vec<OperationSnapshot>> {
        ensure!(
            !note.trim().is_empty() && note.len() <= 2048,
            "record what was reviewed on the original Node"
        );
        let fresh = self.review_settings(&review.operation_id)?;
        ensure!(
            fresh.fingerprint == review.fingerprint
                && fresh.kind == review.kind
                && fresh.fields == review.fields
                && fresh.has_secret_changes == review.has_secret_changes
                && fresh.network_id == review.network_id
                && fresh.client_id == review.client_id
                && fresh.target_node == review.target_node
                && fresh.dependent_ids == review.dependent_ids,
            "settings or dependent drafts changed; review the original group again"
        );
        let operations = self.affected_prepared(&review.operation_id)?;
        let tx = self
            .db
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let mut closed = vec![];
        for mut operation in operations {
            operation.snapshot.state = OperationState::ClosedByUser;
            operation.snapshot.result = Some(
                serde_json::json!({"local_resolution":"closed_by_user","server_outcome":if operation.snapshot.id==review.operation_id {"unknown"}else{"not_sent"},"note":note,"reviewed_at_ms":vibeface_protocol::now_ms(),"parent_operation":review.operation_id}),
            );
            tx.execute(
                "UPDATE operations SET state='closed_by_user',payload=?2 WHERE id=?1",
                params![operation.snapshot.id, serde_json::to_vec(&operation)?],
            )?;
            closed.push(operation.snapshot);
        }
        tx.commit()?;
        Ok(closed)
    }
    pub(crate) fn observe_unknown(&mut self, id: &str, result: Value) -> Result<OperationSnapshot> {
        let mut op = self.get(id)?;
        if op.snapshot.state == OperationState::Unknown {
            op.snapshot.result = Some(result);
            self.save(&op, "unknown")?;
        }
        Ok(op.snapshot)
    }
    pub(crate) fn settle(
        &mut self,
        id: &str,
        state: OperationState,
        result: Value,
    ) -> Result<OperationSnapshot> {
        ensure!(
            matches!(state, OperationState::Accepted | OperationState::Rejected),
            "invalid settlement"
        );
        let mut op = self.get(id)?;
        if matches!(
            op.snapshot.state,
            OperationState::ClosedByUser | OperationState::Accepted | OperationState::Rejected
        ) {
            return Ok(op.snapshot);
        }
        op.snapshot.state = state;
        op.snapshot.result = Some(result);
        self.save(
            &op,
            if state == OperationState::Accepted {
                "accepted"
            } else {
                "rejected"
            },
        )?;
        Ok(op.snapshot)
    }
    fn save(&self, op: &Operation, state: &str) -> Result<()> {
        self.db.execute(
            "UPDATE operations SET state=?2,payload=?3 WHERE id=?1",
            params![op.snapshot.id, state, serde_json::to_vec(op)?],
        )?;
        Ok(())
    }
    pub(crate) fn retain_attachments(
        &mut self,
        owner: &str,
        references: &[Attachment],
    ) -> Result<()> {
        ensure!(
            !owner.is_empty() && owner.len() <= 256,
            "invalid attachment owner"
        );
        ensure!(references.len() <= 128, "too many attachments in a draft");
        for attachment in references {
            self.read_attachment(attachment)?;
        }
        let tx = self
            .db
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let owners: u64 = tx.query_row(
            "SELECT count(DISTINCT owner) FROM attachment_leases",
            [],
            |row| row.get(0),
        )?;
        let exists: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM attachment_leases WHERE owner=?1)",
            [owner],
            |row| row.get(0),
        )?;
        ensure!(
            owners < 256 || exists,
            "too many retained attachment drafts"
        );
        for attachment in references {
            tx.execute(
                "INSERT OR IGNORE INTO attachment_leases VALUES(?1,?2)",
                params![owner, attachment.hash],
            )?;
        }
        tx.commit()?;
        Ok(())
    }
    pub(crate) fn release_attachments(&self, owner: &str) -> Result<()> {
        self.db
            .execute("DELETE FROM attachment_leases WHERE owner=?1", [owner])?;
        Ok(())
    }
    pub(crate) fn attachment_owners(&self) -> Result<Vec<String>> {
        Ok(self
            .db
            .prepare("SELECT DISTINCT owner FROM attachment_leases ORDER BY owner")?
            .query_map([], |row| row.get(0))?
            .collect::<rusqlite::Result<_>>()?)
    }
    /// Content is synced before a SQLite row may reference it. Names never become paths.
    pub fn store_attachment(&self, name: &str, data: &[u8]) -> Result<Attachment> {
        ensure!(
            !name.is_empty()
                && name.len() <= 255
                && !name
                    .chars()
                    .any(|c| c == '/' || c == '\\' || c.is_control()),
            "invalid attachment name"
        );
        ensure!(
            !data.is_empty() && data.len() <= 64 * 1024 * 1024,
            "attachment must be 1 byte to 64 MiB"
        );
        let used = std::fs::read_dir(self.dir.join("attachments"))?
            .try_fold(0u64, |sum, entry| -> Result<u64> {
                Ok(sum + entry?.metadata()?.len())
            })?;
        ensure!(
            used.saturating_add(data.len() as u64) <= 256 * 1024 * 1024,
            "outbox attachment budget exceeded"
        );
        let hash = hex::encode(Sha256::digest(data));
        let path = self.dir.join("attachments").join(&hash);
        if !path.exists() {
            privatefs::write_atomic(&path, data)?;
        }
        let attachment = Attachment {
            hash,
            size: data.len() as u64,
            name: name.into(),
        };
        self.read_attachment(&attachment)?;
        Ok(attachment)
    }
    pub fn read_attachment(&self, attachment: &Attachment) -> Result<Vec<u8>> {
        ensure!(
            attachment.hash.len() == 64 && attachment.hash.bytes().all(|c| c.is_ascii_hexdigit()),
            "invalid attachment hash"
        );
        let data = privatefs::read_private_bounded(
            &self.dir.join("attachments").join(&attachment.hash),
            64 * 1024 * 1024,
            "outbox attachment",
        )?;
        ensure!(
            data.len() as u64 == attachment.size
                && hex::encode(Sha256::digest(&data)) == attachment.hash,
            "attachment content mismatch"
        );
        Ok(data)
    }
    /// Explicit cleanup runs with the application's outbox lock held. Referenced bytes survive restart.
    pub fn collect_unreferenced(&self) -> Result<()> {
        self.collect_attachments(&[])
    }
    pub fn collect_attachments(&self, keep: &[Attachment]) -> Result<()> {
        let mut hashes: std::collections::HashSet<_> = self
            .operations()?
            .into_iter()
            .filter(|op| op.snapshot.state != OperationState::Accepted)
            .flat_map(|op| op.attachments.into_iter().map(|a| a.hash))
            .collect();
        for attachment in keep {
            self.read_attachment(attachment)?;
            hashes.insert(attachment.hash.clone());
        }
        let mut leases = self
            .db
            .prepare("SELECT DISTINCT hash FROM attachment_leases")?;
        for hash in leases.query_map([], |row| row.get::<_, String>(0))? {
            hashes.insert(hash?);
        }
        for entry in std::fs::read_dir(self.dir.join("attachments"))? {
            let entry = entry?;
            if !hashes.contains(&entry.file_name().to_string_lossy().to_string())
                && entry.file_type()?.is_file()
            {
                std::fs::remove_file(entry.path())?;
            }
        }
        privatefs::sync_dir(&self.dir.join("attachments"))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn operation(id: &str) -> Operation {
        Operation {
            snapshot:OperationSnapshot {id:id.into(),network_id:"network-a".into(),client_id:"client-a".into(),target_node:"node-a".into(),state:OperationState::Prepared,sequence:Some(1),result:None},
            lane:"chat-a".into(),expected_revision:Some(7),bytes:br#"{"id":"business-1","method":"rpc","params":{"attestation":{"signature":"exact-original"}}}"#.to_vec(),
            reconciliation:Reconciliation::Manual,attachments:vec![],upload_scope:None,prerequisites:vec![],
        }
    }
    #[test]
    fn crash_keeps_original_bytes_and_blocks_the_lane_and_other_identity() -> Result<()> {
        let dir = std::env::temp_dir().join(format!("native-outbox-{}", uuid::Uuid::new_v4()));
        let mut db = Outbox::open(&dir, "network-a", "client-a")?;
        let mut original = operation("business-1");
        let attachment = db.store_attachment("note.txt", b"durable attachment")?;
        original.attachments.push(attachment.clone());
        db.prepare(&original)?;
        db.begin("business-1")?;
        drop(db);
        assert!(Outbox::open(&dir, "network-b", "client-a").is_err());
        assert!(Outbox::open(&dir, "network-a", "client-b").is_err());
        let mut recovered = Outbox::open(&dir, "network-a", "client-a")?;
        assert_eq!(recovered.get("business-1")?.bytes, original.bytes);
        assert_eq!(
            recovered.get("business-1")?.snapshot.state,
            OperationState::Unknown
        );
        assert!(recovered.begin("business-1").is_err());
        assert!(recovered.prepare(&operation("business-2")).is_err());
        assert_eq!(
            recovered.read_attachment(&attachment)?,
            b"durable attachment"
        );
        let mut switched = original.clone();
        switched.snapshot.target_node = "node-b".into();
        assert!(recovered.prepare(&switched).is_err());
        recovered.collect_unreferenced()?;
        assert_eq!(
            recovered.read_attachment(&attachment)?,
            b"durable attachment"
        );
        drop(recovered);
        std::fs::remove_dir_all(dir)?;
        Ok(())
    }
    #[test]
    fn content_commits_before_reference_and_unreferenced_files_are_collected() -> Result<()> {
        let dir = std::env::temp_dir().join(format!("native-attachments-{}", uuid::Uuid::new_v4()));
        let mut db = Outbox::open(&dir, "network-a", "client-a")?;
        assert!(db.store_attachment("../escape", b"x").is_err());
        let lost = db.store_attachment("orphan.txt", b"orphan")?;
        db.collect_unreferenced()?;
        assert!(db.read_attachment(&lost).is_err());
        let mut op = operation("missing-bytes");
        op.attachments.push(lost);
        assert!(db.prepare(&op).is_err());
        assert!(db.snapshots()?.is_empty());
        assert!(Outbox::open(&dir, "network-a", "client-a").is_err());
        drop(db);
        std::fs::remove_dir_all(dir)?;
        Ok(())
    }
}

#[cfg(test)]
mod lease_tests {
    use super::*;
    #[test]
    fn draft_lease_survives_a_crash_before_the_ui_sidecar_or_outbox_reference() -> Result<()> {
        let dir = std::env::temp_dir().join(format!("native-draft-lease-{}", uuid::Uuid::new_v4()));
        let mut db = Outbox::open(&dir, "network", "client")?;
        let file = db.store_attachment("draft.txt", b"must survive")?;
        let orphan = db.store_attachment("orphan.txt", b"not referenced")?;
        db.retain_attachments("message-draft", std::slice::from_ref(&file))?;
        drop(db);
        let db = Outbox::open(&dir, "network", "client")?;
        db.collect_attachments(&[])?;
        assert_eq!(db.read_attachment(&file)?, b"must survive");
        assert!(db.read_attachment(&orphan).is_err());
        assert_eq!(db.attachment_owners()?, vec!["message-draft"]);
        db.release_attachments("message-draft")?;
        db.collect_attachments(&[])?;
        assert!(db.read_attachment(&file).is_err());
        drop(db);
        std::fs::remove_dir_all(dir)?;
        Ok(())
    }
}
