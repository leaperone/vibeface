//! Reads and local records for Node-to-Node sync: log ranges, heads, the signed address book,
//! scheduler deliveries and resumable attachment downloads.
use super::{
    Store, log, message_from,
    scheduler::{admit_from, admit_work_result_from},
};
use crate::{
    model::*,
    net::{AddressRecord, SignedAddress},
    scheduling::{AdmissionReceipt, TriggerKind},
};
use anyhow::{Context as _, Result, ensure};
use rusqlite::{OptionalExtension, TransactionBehavior, params};
use std::{collections::BTreeMap, path::PathBuf};

pub const MAX_PULL_EVENTS: usize = 1024;
pub const MAX_PULL_BYTES: usize = 8 * 1024 * 1024;

impl Store {
    /// `{author: (max_seq, head_hash)}`.
    pub fn heads(&self, network: &str) -> Result<BTreeMap<String, (u64, String)>> {
        let mut statement = self
            .connection
            .prepare("SELECT author,max_seq,head_hash FROM log_heads WHERE network_id=?")?;
        let rows = statement.query_map([network], |r| Ok((r.get(0)?, (r.get(1)?, r.get(2)?))))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn hash_at(&self, network: &str, author: &str, seq: u64) -> Result<Option<String>> {
        Ok(self
            .connection
            .query_row(
                "SELECT hash FROM events WHERE network_id=? AND author=? AND seq=? AND main=1",
                params![network, author, seq],
                |r| r.get(0),
            )
            .optional()?)
    }

    pub fn has_conflicts_by(&self, network: &str, author: &str) -> Result<bool> {
        Ok(self
            .connection
            .query_row(
                "SELECT 1 FROM events WHERE network_id=? AND author=? AND main=0 LIMIT 1",
                params![network, author],
                |_| Ok(()),
            )
            .optional()?
            .is_some())
    }

    /// The first fork events, each beside the main-chain event at the same place. A removed
    /// device's fork is settled by its revocation cutoff, so it is left out.
    pub fn conflicts(&self, network: &str) -> Result<Vec<serde_json::Value>> {
        let view = super::membership_from(&self.connection, network)?;
        let mut statement = self.connection.prepare(
            "SELECT c.author,c.seq,c.envelope,e.envelope FROM events c LEFT JOIN events e
             ON e.network_id=c.network_id AND e.author=c.author AND e.seq=c.seq AND e.main=1
             WHERE c.network_id=? AND c.main=0 ORDER BY c.author,c.seq,c.hash",
        )?;
        let rows = statement.query_map([network], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, u64>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, Option<String>>(3)?,
            ))
        })?;
        let version = |envelope: &str| -> Result<serde_json::Value> {
            let envelope: SignedEvent = serde_json::from_str(envelope)?;
            let event: Event = serde_json::from_str(&envelope.canonical_body)?;
            Ok(
                serde_json::json!({"hash":envelope.hash,"scope_id":event.scope_id,
                "created_at_ms":event.created_at_ms,"body":event.body}),
            )
        };
        let mut conflicts = Vec::new();
        for row in rows {
            let (author, seq, other, kept) = row?;
            if view.revocation(&author).is_some() {
                continue;
            }
            if conflicts.len() == 20 {
                break;
            }
            conflicts.push(serde_json::json!({"author":author,"seq":seq,
                "kept":kept.as_deref().map(version).transpose()?,"other":version(&other)?}));
        }
        Ok(conflicts)
    }

    /// One author's main-chain events from `from_seq` on, within the batch limits.
    pub fn events_from(
        &self,
        network: &str,
        author: &str,
        from_seq: u64,
        limit: usize,
    ) -> Result<Vec<SignedEvent>> {
        let mut statement = self.connection.prepare(
            "SELECT envelope FROM events WHERE network_id=? AND author=? AND seq>=? AND main=1 ORDER BY seq LIMIT ?",
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

    /// `(author, seq, hash)` of every event from each forked author's first fork on, on every
    /// branch: the fork evidence two Nodes compare. Which branch is main differs by replica, so
    /// it is left out.
    fn fork_keys(&self, network: &str) -> Result<Vec<(String, u64, String)>> {
        let forks: Vec<(String, u64)> = self
            .connection
            .prepare(
                "SELECT author,min(seq) FROM events WHERE network_id=? AND main=0 GROUP BY author",
            )?
            .query_map([network], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<rusqlite::Result<_>>()?;
        let mut keys = Vec::new();
        for sql in [
            "SELECT seq,hash FROM events WHERE network_id=? AND author=? AND seq>=? AND main=1",
            "SELECT seq,hash FROM events WHERE network_id=? AND author=? AND seq>=? AND main=0",
        ] {
            let mut statement = self.connection.prepare(sql)?;
            for (author, fork) in &forks {
                for row in statement.query_map(params![network, author, fork], |r| {
                    Ok((r.get::<_, u64>(0)?, r.get::<_, String>(1)?))
                })? {
                    let (seq, hash) = row?;
                    keys.push((author.clone(), seq, hash));
                }
            }
        }
        keys.sort();
        Ok(keys)
    }

    /// Empty without forks; otherwise differs whenever two Nodes hold different fork evidence.
    pub fn fork_digest(&self, network: &str) -> Result<String> {
        let keys = self.fork_keys(network)?;
        if keys.is_empty() {
            return Ok(String::new());
        }
        let hashes: Vec<&str> = keys.iter().map(|(_, _, hash)| hash.as_str()).collect();
        Ok(crate::crypto::digest(hashes.join(",").as_bytes()))
    }

    /// The fork evidence after `after` in `(author, seq, hash)` order, within the batch limits.
    pub fn fork_events(
        &self,
        network: &str,
        after: Option<&(String, u64, String)>,
    ) -> Result<Vec<SignedEvent>> {
        let mut events = Vec::new();
        let mut bytes = 0;
        for (_, _, hash) in self
            .fork_keys(network)?
            .iter()
            .filter(|key| after.is_none_or(|after| *key > after))
            .take(MAX_PULL_EVENTS)
        {
            let envelope: String = self.connection.query_row(
                "SELECT envelope FROM events WHERE network_id=? AND hash=?",
                params![network, hash],
                |r| r.get(0),
            )?;
            bytes += envelope.len();
            if bytes > MAX_PULL_BYTES && !events.is_empty() {
                break;
            }
            events.push(serde_json::from_str(&envelope)?);
        }
        Ok(events)
    }

    pub fn address_book(&self) -> Result<Vec<SignedAddress>> {
        let mut statement = self
            .connection
            .prepare("SELECT record FROM addresses ORDER BY device_id")?;
        let rows = statement.query_map([], |r| r.get::<_, String>(0))?;
        rows.map(|row| Ok(serde_json::from_str(&row?)?)).collect()
    }

    /// Keeps the higher `seq` per device. Returns whether the stored record changed.
    pub fn save_address(
        &mut self,
        network: &str,
        record: &AddressRecord,
        signed: &SignedAddress,
    ) -> Result<bool> {
        let existing: Option<String> = self
            .connection
            .query_row(
                "SELECT record FROM addresses WHERE device_id=?",
                [&record.device_id],
                |r| r.get(0),
            )
            .optional()?;
        if let Some(existing) = existing
            && serde_json::from_str::<SignedAddress>(&existing)?
                .open(network)?
                .seq
                >= record.seq
        {
            return Ok(false);
        }
        self.connection.execute(
            "INSERT INTO addresses(device_id,record) VALUES(?,?) ON CONFLICT(device_id) DO UPDATE SET record=excluded.record",
            params![record.device_id, serde_json::to_string(signed)?],
        )?;
        Ok(true)
    }

    pub fn forget_address(&mut self, device_id: &str) -> Result<()> {
        self.connection
            .execute("DELETE FROM addresses WHERE device_id=?", [device_id])?;
        Ok(())
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

    /// The owner admits a user message or a work result another Node holds. Idempotent by
    /// trigger, so a second delivery gets the original receipt. `None` means the triggering
    /// event has not been synced here yet.
    pub fn admit_delivery(
        &mut self,
        local: &str,
        network: &str,
        conversation: &str,
        trigger: &str,
    ) -> Result<Option<AdmissionReceipt>> {
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
        let receipt = match trigger.split_once(':') {
            Some(("message", message_id)) => {
                let Some(message) = message_from(&tx, network, conversation, message_id)? else {
                    return Ok(None);
                };
                ensure!(
                    message.role == Role::User,
                    "only user messages are delivered"
                );
                let kind = match message.answers {
                    Some(_) => TriggerKind::Answer,
                    None => TriggerKind::Human,
                };
                admit_from(&tx, network, conversation, trigger, kind, message_id)?
            }
            Some(("work", work_id)) => {
                let outcome = super::reduce::works_from(&tx, network, conversation)?
                    .into_iter()
                    .find(|work| work.work_id == work_id)
                    .and_then(|work| work.outcome);
                let Some(outcome) = outcome.filter(|o| *o != WorkOutcome::Unverified) else {
                    return Ok(None);
                };
                // A result the user asked not to report is delivered all the same.
                admit_work_result_from(&tx, network, local, conversation, work_id, outcome)?
                    .unwrap_or(AdmissionReceipt {
                        network_id: network.into(),
                        conversation_id: conversation.into(),
                        trigger: trigger.into(),
                        state: "not_reported".into(),
                    })
            }
            // A routing turn follows its conversation to a new owner.
            Some(("route", question)) => {
                if super::questions::question_from(&tx, network, question)?.is_none() {
                    return Ok(None);
                }
                admit_from(
                    &tx,
                    network,
                    conversation,
                    trigger,
                    TriggerKind::Route,
                    question,
                )?
            }
            _ => anyhow::bail!("only user messages, work results and routing turns are delivered"),
        };
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
    pub fn committed_offset(&self, hash: &str) -> Result<u64> {
        Ok(self
            .connection
            .query_row(
                "SELECT committed FROM blob_offsets WHERE hash=?",
                [hash],
                |r| r.get(0),
            )
            .optional()?
            .unwrap_or(0))
    }

    pub fn commit_offset(&mut self, hash: &str, offset: Option<u64>) -> Result<()> {
        match offset {
            Some(offset) => self.connection.execute(
                "INSERT INTO blob_offsets(hash,committed) VALUES(?,?) ON CONFLICT(hash) DO UPDATE SET committed=excluded.committed",
                params![hash, offset],
            )?,
            None => self
                .connection
                .execute("DELETE FROM blob_offsets WHERE hash=?", [hash])?,
        };
        Ok(())
    }
}
