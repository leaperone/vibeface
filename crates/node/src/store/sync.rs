//! Reads and local records for Node-to-Node sync: log ranges, heads, the signed address book,
//! scheduler deliveries and resumable attachment downloads.
use super::{Store, local_load, local_prefix, local_save, log, message_from, scheduler::admit_from};
use crate::{
    model::*,
    net::{AddressRecord, SignedAddress},
    scheduling::{AdmissionReceipt, TriggerKind, scheduler_key},
};
use anyhow::{Context as _, Result, ensure};
use rusqlite::{OptionalExtension, TransactionBehavior, params};
use std::{collections::BTreeMap, path::PathBuf};

pub const MAX_PULL_EVENTS: usize = 1024;
pub const MAX_PULL_BYTES: usize = 8 * 1024 * 1024;
const ADDRESS_PREFIX: &str = "address:";
const PARTIAL_PREFIX: &str = "blob-partial:";

impl Store {
    /// `{author: (max_seq, head_hash)}`.
    pub fn heads(&self, network: &str) -> Result<BTreeMap<String, (u64, String)>> {
        let mut statement = self
            .connection
            .prepare("SELECT author,max_seq,head_hash FROM log_heads WHERE network_id=?")?;
        let rows = statement.query_map([network], |r| Ok((r.get(0)?, (r.get(1)?, r.get(2)?))))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// One author's events from `from_seq` on, within the batch limits.
    pub fn events_from(
        &self,
        network: &str,
        author: &str,
        from_seq: u64,
        limit: usize,
    ) -> Result<Vec<SignedEvent>> {
        let mut statement = self.connection.prepare(
            "SELECT envelope FROM events WHERE network_id=? AND author=? AND seq>=? ORDER BY seq LIMIT ?",
        )?;
        let rows = statement.query_map(
            params![network, author, from_seq, limit.min(MAX_PULL_EVENTS) as i64],
            |r| r.get::<_, String>(0),
        )?;
        let mut events = Vec::new();
        let mut bytes = 0;
        for row in rows {
            let row = row?;
            bytes += row.len();
            if bytes > MAX_PULL_BYTES && !events.is_empty() {
                break;
            }
            events.push(serde_json::from_str(&row)?);
        }
        Ok(events)
    }

    pub fn address_book(&self, network: &str) -> Result<Vec<SignedAddress>> {
        local_prefix(&self.connection, network, ADDRESS_PREFIX)
    }

    /// Keeps the higher `seq` per device. Returns whether the stored record changed.
    pub fn save_address(
        &mut self,
        network: &str,
        record: &AddressRecord,
        signed: &SignedAddress,
    ) -> Result<bool> {
        let key = format!("{ADDRESS_PREFIX}{}", record.device_id);
        if let Some(existing) = local_load::<SignedAddress>(&self.connection, network, &key)?
            && existing.open(network)?.seq >= record.seq
        {
            return Ok(false);
        }
        local_save(&self.connection, network, &key, signed)?;
        Ok(true)
    }

    pub fn forget_address(&mut self, network: &str, device_id: &str) -> Result<()> {
        self.local_delete(network, &format!("{ADDRESS_PREFIX}{device_id}"))
    }

    /// Whether `token` belongs to an invite registered here that can still be used.
    pub fn relay_token_open(&self, token: &str) -> Result<bool> {
        let mut statement = self.connection.prepare(
            "SELECT invite_hash FROM invites WHERE expires_at_ms>? AND (status IS NULL OR status IN ('pending','confirming'))",
        )?;
        let hashes = statement.query_map([crate::runtime::now_ms()], |r| r.get::<_, String>(0))?;
        for hash in hashes {
            if crate::net::relay_token(&hash?) == token {
                return Ok(true);
            }
        }
        Ok(false)
    }

    pub fn remove_outbox(&mut self, network: &str, conversation: &str, trigger: &str) -> Result<()> {
        self.local_delete(network, &scheduler_key("outbox", conversation, trigger))
    }

    /// The owner admits a user message another Node accepted. Idempotent by trigger, so a
    /// second entry delivering the same message gets the original receipt.
    pub fn admit_delivery(
        &mut self,
        local: &str,
        network: &str,
        conversation: &str,
        trigger: &str,
    ) -> Result<Option<AdmissionReceipt>> {
        let message_id = trigger
            .strip_prefix("message:")
            .context("only user messages are delivered")?;
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let owner = super::conversation_from(&tx, network, conversation)?
            .context("conversation not found")?
            .owner;
        ensure!(
            owner.as_deref() == Some(local),
            "this Node does not schedule the conversation"
        );
        let Some(message) = message_from(&tx, network, conversation, message_id)? else {
            return Ok(None);
        };
        ensure!(message.role == Role::User, "only user messages are delivered");
        let receipt = admit_from(
            &tx,
            network,
            conversation,
            trigger,
            TriggerKind::Human,
            message_id,
        )?;
        tx.commit()?;
        Ok(Some(receipt))
    }

    /// Size of an attachment some stored event references; a source serves nothing else.
    pub fn referenced_blob(&self, network: &str, hash: &str) -> Result<Option<u64>> {
        Ok(self
            .connection
            .query_row(
                "SELECT size FROM event_blobs WHERE network_id=? AND blob=? LIMIT 1",
                params![network, hash],
                |r| r.get(0),
            )
            .optional()?)
    }

    /// Attachments referenced by stored events whose file is not here yet.
    pub fn missing_blobs(&self, network: &str) -> Result<Vec<BlobRef>> {
        let mut statement = self
            .connection
            .prepare("SELECT DISTINCT blob,size FROM event_blobs WHERE network_id=?")?;
        let rows = statement.query_map([network], |r| {
            Ok(BlobRef {
                hash: r.get(0)?,
                size: r.get(1)?,
            })
        })?;
        let mut missing = Vec::new();
        for blob in rows {
            let blob = blob?;
            if !log::blob_file_matches(&self.blob_file(&blob.hash)?, &blob) {
                missing.push(blob);
            }
        }
        Ok(missing)
    }

    pub fn blob_file(&self, hash: &str) -> Result<PathBuf> {
        self.blob_path(hash)
    }

    pub fn partial_blob_file(&self, hash: &str) -> Result<PathBuf> {
        self.blob_path(hash)?;
        Ok(self.blob_dir.join("partial").join(format!("{hash}.part")))
    }

    /// Bytes of a download that were synced to disk; anything after this is discarded.
    pub fn committed_offset(&self, network: &str, hash: &str) -> Result<u64> {
        Ok(
            local_load(&self.connection, network, &format!("{PARTIAL_PREFIX}{hash}"))?
                .unwrap_or(0),
        )
    }

    pub fn commit_offset(&mut self, network: &str, hash: &str, offset: Option<u64>) -> Result<()> {
        let key = format!("{PARTIAL_PREFIX}{hash}");
        match offset {
            Some(offset) => local_save(&self.connection, network, &key, &offset),
            None => self.local_delete(network, &key),
        }
    }
}
