//! This Node's membership record, its one-time bootstrap capability, and join requests.
//!
//! A ticket's secret never reaches this database; invites are stored by `SHA256(secret)`.
//! A join request moves `pending → confirming → joined`, or ends `rejected` or `expired`.

use super::{AuthorizationView, GovernanceAction, GovernanceEvent, IdentityKind, NetworkState};
use crate::crypto;
use crate::store::Store;
use anyhow::{Context, Result, bail, ensure};
use rand::{RngCore, rngs::OsRng};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use serde::{Deserialize, Serialize};
use std::time::{SystemTime, UNIX_EPOCH};
use vibeface_secure::join::{InviteRecord, TICKET_LIFETIME_MS, invite_id, join_commit};

const MAX_PENDING_INVITES: i64 = 32;
const CLOCK_SKEW_MS: i64 = 60_000;

#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct BootstrapCapability {
    pub capability: String,
}
impl std::fmt::Debug for BootstrapCapability {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BootstrapCapability")
            .field("capability", &"[redacted]")
            .finish()
    }
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum JoinStatus {
    Pending,
    Confirming,
    Joined,
    Rejected,
    Expired,
}

/// What the joining device and management devices may see of one request.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct JoinRequest {
    pub join_id: String,
    pub status: JoinStatus,
    pub network_id: String,
    pub invite_id: String,
    pub device_id: String,
    pub kind: IdentityKind,
    pub name: String,
    pub admin_id: Option<String>,
    pub admin_nonce: Option<String>,
    pub joiner_nonce: Option<String>,
    pub reason: Option<String>,
    pub expires_at_ms: i64,
}

/// The joining device's request; `commit` is `SHA256(device_id ‖ joiner_nonce)`.
#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Join {
    pub secret: String,
    pub public_key: String,
    pub kind: IdentityKind,
    pub name: String,
    pub commit: String,
}

impl Store {
    fn transaction<T>(
        &self,
        operation: impl FnOnce(&rusqlite::Transaction<'_>) -> Result<T>,
    ) -> Result<T> {
        let tx =
            rusqlite::Transaction::new_unchecked(&self.connection, TransactionBehavior::Immediate)?;
        let value = operation(&tx)?;
        tx.commit()?;
        Ok(value)
    }

    pub fn bootstrap_capability(&self) -> Result<BootstrapCapability> {
        let capability = random_hex(32);
        self.transaction(|tx| {
            ensure!(!has_network_in(tx)?, "network already installed");
            let issued: i64 = tx.query_row(
                "SELECT COUNT(*) FROM identity_meta WHERE key='bootstrap_capability_hash'",
                [],
                |r| r.get(0),
            )?;
            ensure!(issued == 0, "bootstrap capability already issued");
            tx.execute(
                "INSERT INTO identity_meta(key,value) VALUES('bootstrap_capability_hash',?)",
                [crypto::digest(capability.as_bytes())],
            )?;
            Ok(())
        })?;
        Ok(BootstrapCapability { capability })
    }

    pub fn verify_bootstrap_capability(&self, capability: &str) -> Result<()> {
        verify_bootstrap_in(&self.connection, capability)
    }

    /// Installs a genesis, or a recovered network, on a Node that has none yet.
    pub fn bootstrap_network(
        &self,
        state: NetworkState,
        capability: &str,
        client: &str,
    ) -> Result<AuthorizationView> {
        ensure!(
            state.events.is_empty(),
            "bootstrap accepts genesis or a checkpoint"
        );
        let view = state.project()?;
        view.manager(client)?;
        self.transaction(|tx| {
            verify_bootstrap_in(tx, capability)?;
            save_state(tx, &state)?;
            tx.execute(
                "UPDATE identity_meta SET value='consumed:' || value WHERE key='bootstrap_capability_hash'",
                [],
            )?;
            Ok(())
        })?;
        Ok(view)
    }

    pub fn init_network(&self, state: NetworkState) -> Result<AuthorizationView> {
        let view = state.project()?;
        self.transaction(|tx| {
            ensure!(!has_network_in(tx)?, "network already installed");
            save_state(tx, &state)
        })?;
        Ok(view)
    }

    pub fn public_state(&self) -> Result<NetworkState> {
        let state = load_state(&self.connection)?;
        state.project()?;
        Ok(state)
    }

    pub fn has_network(&self) -> Result<bool> {
        has_network_in(&self.connection)
    }

    /// Merges `incoming`; a higher recovery epoch voids every invite and join request.
    pub fn install_state(&mut self, incoming: NetworkState) -> Result<AuthorizationView> {
        incoming.project()?;
        self.transaction(|tx| {
            let next = if has_network_in(tx)? {
                let current = load_state(tx)?;
                let merged = current.merge(&incoming)?;
                if merged.epoch() > current.epoch() {
                    tx.execute("DELETE FROM invites", [])?;
                }
                merged
            } else {
                incoming
            };
            let view = next.project()?;
            save_state(tx, &next)?;
            Ok(view)
        })
    }

    /// Registers a manager-signed invite; registering the same record again is a no-op.
    pub fn register_invite(&self, record: &InviteRecord, issuer: &str) -> Result<String> {
        record.verify().map_err(anyhow::Error::msg)?;
        ensure!(
            record.issuer == issuer,
            "invite signer differs from this session"
        );
        let now = now_ms();
        ensure!(
            record.expires_at_ms > now
                && record.expires_at_ms <= now + TICKET_LIFETIME_MS + CLOCK_SKEW_MS,
            "invalid invite expiry"
        );
        let join_id = invite_id(&record.invite_hash).map_err(anyhow::Error::msg)?;
        self.transaction(|tx| {
            let view = load_state(tx)?.project()?;
            ensure!(record.network_id == view.network_id, "invite belongs to another network");
            view.manager(issuer)?;
            let encoded = serde_json::to_string(record)?;
            if let Some(existing) = tx
                .query_row(
                    "SELECT record FROM invites WHERE invite_hash=?",
                    [&record.invite_hash],
                    |r| r.get::<_, String>(0),
                )
                .optional()?
            {
                ensure!(existing == encoded, "invite was registered with another record");
                return Ok(());
            }
            let pending: i64 = tx.query_row(
                "SELECT COUNT(*) FROM invites WHERE status IS NULL AND expires_at_ms>?",
                [now],
                |r| r.get(0),
            )?;
            ensure!(pending < MAX_PENDING_INVITES, "too many pending invites");
            tx.execute(
                "INSERT INTO invites(join_id,invite_hash,issuer,epoch,expires_at_ms,record) VALUES(?,?,?,?,?,?)",
                params![join_id, record.invite_hash, issuer, view.epoch, record.expires_at_ms, encoded],
            )?;
            Ok(())
        })?;
        Ok(join_id)
    }

    /// Consumes the ticket for one device; the same request retried returns the same `join_id`.
    pub fn join(&self, join: &Join, device: &str) -> Result<JoinRequest> {
        ensure!(
            join.public_key == device,
            "join key differs from this session"
        );
        super::validate_name(&join.name)?;
        super::validate_hash(&join.commit)?;
        let secret: [u8; 32] = hex::decode(&join.secret)
            .ok()
            .and_then(|bytes| bytes.try_into().ok())
            .context("invalid ticket secret")?;
        let invite_hash = vibeface_secure::join::invite_hash(&secret);
        let kind = serde_json::to_value(join.kind)?;
        let kind = kind.as_str().context("invalid device kind")?;
        let now = now_ms();
        self.transaction(|tx| {
            let view = load_state(tx)?.project()?;
            let row = load_request(tx, "invite_hash", &invite_hash)?
                .context("ticket is unknown or already expired")?;
            if row.status.is_some() {
                ensure!(
                    row.device_id.as_deref() == Some(device)
                        && row.kind.as_deref() == Some(kind)
                        && row.name.as_deref() == Some(join.name.as_str())
                        && row.commit.as_deref() == Some(join.commit.as_str()),
                    "ticket has already been used"
                );
                return row.view(&view, now);
            }
            ensure!(row.expires_at_ms > now && row.epoch == view.epoch, "ticket has expired");
            ensure!(
                view.cutoff(device).is_none() && view.member(device).is_err(),
                "device is already a member or was revoked"
            );
            tx.execute(
                "UPDATE invites SET device_id=?,kind=?,name=?,commit_hash=?,status='pending',expires_at_ms=? WHERE join_id=?",
                params![device, kind, join.name, join.commit, now + TICKET_LIFETIME_MS, row.join_id],
            )?;
            load_request(tx, "join_id", &row.join_id)?
                .context("join request vanished")?
                .view(&view, now)
        })
    }

    /// The joining device's view; `reveal` opens its commitment once the admin nonce is known.
    pub fn join_status(
        &self,
        join_id: &str,
        device: &str,
        reveal: Option<&str>,
    ) -> Result<JoinRequest> {
        let now = now_ms();
        self.transaction(|tx| {
            let view = load_state(tx)?.project()?;
            let row = load_request(tx, "join_id", join_id)?.context("join request not found")?;
            ensure!(row.device_id.as_deref() == Some(device), "join request belongs to another device");
            let current = row.view(&view, now)?;
            if let Some(nonce) = reveal {
                match (&row.joiner_nonce, current.status) {
                    (Some(old), _) => ensure!(old == nonce, "a different nonce was already revealed"),
                    (None, JoinStatus::Confirming) => {
                        let matches = super::validate_hash(nonce).is_ok()
                            && join_commit(device, nonce).ok().as_deref() == row.commit.as_deref();
                        if matches {
                            tx.execute("UPDATE invites SET joiner_nonce=? WHERE join_id=?", params![nonce, join_id])?;
                        } else {
                            tx.execute(
                                "UPDATE invites SET status='rejected',reason='commitment_mismatch' WHERE join_id=?",
                                [join_id],
                            )?;
                        }
                    }
                    (None, _) => bail!("nothing to reveal in this state"),
                }
                return load_request(tx, "join_id", join_id)?
                    .context("join request vanished")?
                    .view(&view, now);
            }
            Ok(current)
        })
    }

    /// Requests a management device may approve; any manager may answer any request.
    pub fn pending_joins(&self, admin: &str) -> Result<Vec<JoinRequest>> {
        let view = load_state(&self.connection)?.project()?;
        view.manager(admin)?;
        let now = now_ms();
        let mut statement = self.connection.prepare(
            "SELECT join_id FROM invites WHERE status IN ('pending','confirming') AND expires_at_ms>? ORDER BY expires_at_ms",
        )?;
        let ids = statement
            .query_map([now], |r| r.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        ids.iter()
            .filter_map(|id| load_request(&self.connection, "join_id", id).transpose())
            .map(|row| row?.view(&view, now))
            .collect()
    }

    /// First approval step: this manager's fresh nonce, sent only after the commitment exists.
    pub fn approve_nonce(&self, join_id: &str, admin: &str, nonce: &str) -> Result<JoinRequest> {
        super::validate_hash(nonce)?;
        let now = now_ms();
        self.transaction(|tx| {
            let view = load_state(tx)?.project()?;
            view.manager(admin)?;
            let row = load_request(tx, "join_id", join_id)?.context("join request not found")?;
            let current = row.view(&view, now)?;
            match current.status {
                JoinStatus::Pending => {
                    tx.execute(
                        "UPDATE invites SET status='confirming',admin_id=?,admin_nonce=? WHERE join_id=?",
                        params![admin, nonce, join_id],
                    )?;
                    load_request(tx, "join_id", join_id)?
                        .context("join request vanished")?
                        .view(&view, now)
                }
                JoinStatus::Confirming
                    if row.admin_id.as_deref() == Some(admin)
                        && row.admin_nonce.as_deref() == Some(nonce) =>
                {
                    Ok(current)
                }
                _ => bail!("join request is not waiting for this approval"),
            }
        })
    }

    /// Second approval step: the manager's signed grant for exactly the requested device.
    pub fn approve_grant(
        &self,
        join_id: &str,
        admin: &str,
        event: GovernanceEvent,
    ) -> Result<(JoinRequest, NetworkState)> {
        let now = now_ms();
        let hash = event.hash()?;
        self.transaction(|tx| {
            let mut state = load_state(tx)?;
            let view = state.project()?;
            let row = load_request(tx, "join_id", join_id)?.context("join request not found")?;
            let current = row.view(&view, now)?;
            if current.status == JoinStatus::Joined {
                ensure!(
                    row.grant_hash.as_deref() == Some(hash.as_str()),
                    "join completed with another grant"
                );
                return Ok((current, state));
            }
            ensure!(
                current.status == JoinStatus::Confirming && row.joiner_nonce.is_some(),
                "the new device has not confirmed its short code yet"
            );
            ensure!(
                event.issuer == admin && row.admin_id.as_deref() == Some(admin),
                "only the manager that started the confirmation can approve it"
            );
            let GovernanceAction::Grant { member } = &event.action else {
                bail!("approval requires a grant")
            };
            ensure!(
                member.device_id == current.device_id
                    && member.kind == current.kind
                    && member.name == current.name,
                "grant differs from the join request"
            );
            state.append(event)?;
            ensure!(
                state.project()?.member(&current.device_id).is_ok(),
                "grant did not admit the device"
            );
            save_state(tx, &state)?;
            tx.execute(
                "UPDATE invites SET status='joined',grant_hash=? WHERE join_id=?",
                params![hash, join_id],
            )?;
            let joined = load_request(tx, "join_id", join_id)?
                .context("join request vanished")?
                .view(&state.project()?, now)?;
            Ok((joined, state))
        })
    }

    /// Whether this Node holds the invite the join came through.
    pub fn holds_join(&self, join_id: &str) -> Result<bool> {
        Ok(load_request(&self.connection, "join_id", join_id)?.is_some())
    }

    pub fn reject_join(&self, join_id: &str, admin: &str) -> Result<JoinRequest> {
        let now = now_ms();
        self.transaction(|tx| {
            let view = load_state(tx)?.project()?;
            view.manager(admin)?;
            let row = load_request(tx, "join_id", join_id)?.context("join request not found")?;
            match row.view(&view, now)?.status {
                JoinStatus::Pending | JoinStatus::Confirming => {
                    tx.execute(
                        "UPDATE invites SET status='rejected',reason='rejected' WHERE join_id=?",
                        [join_id],
                    )?;
                }
                JoinStatus::Rejected => {}
                _ => bail!("join request can no longer be rejected"),
            }
            load_request(tx, "join_id", join_id)?
                .context("join request vanished")?
                .view(&view, now)
        })
    }
}

struct Row {
    join_id: String,
    epoch: u64,
    expires_at_ms: i64,
    device_id: Option<String>,
    kind: Option<String>,
    name: Option<String>,
    commit: Option<String>,
    status: Option<String>,
    admin_id: Option<String>,
    admin_nonce: Option<String>,
    joiner_nonce: Option<String>,
    reason: Option<String>,
    grant_hash: Option<String>,
}

impl Row {
    fn view(&self, view: &AuthorizationView, now: i64) -> Result<JoinRequest> {
        let stored = self.status.as_deref().context("ticket has not been used")?;
        let mut status: JoinStatus = serde_json::from_value(serde_json::json!(stored))?;
        if matches!(status, JoinStatus::Pending | JoinStatus::Confirming)
            && (self.expires_at_ms <= now || self.epoch != view.epoch)
        {
            status = JoinStatus::Expired;
        }
        Ok(JoinRequest {
            join_id: self.join_id.clone(),
            status,
            network_id: view.network_id.clone(),
            invite_id: self.join_id.clone(),
            device_id: self.device_id.clone().context("join device missing")?,
            kind: serde_json::from_value(serde_json::json!(self.kind))?,
            name: self.name.clone().unwrap_or_default(),
            admin_id: self.admin_id.clone(),
            admin_nonce: self.admin_nonce.clone(),
            joiner_nonce: self.joiner_nonce.clone(),
            reason: self.reason.clone(),
            expires_at_ms: self.expires_at_ms,
        })
    }
}

fn load_request(conn: &Connection, column: &str, value: &str) -> Result<Option<Row>> {
    let column = match column {
        "join_id" => "join_id",
        "invite_hash" => "invite_hash",
        _ => bail!("invalid join lookup"),
    };
    Ok(conn
        .query_row(
            &format!(
                "SELECT join_id,epoch,expires_at_ms,device_id,kind,name,commit_hash,status,admin_id,admin_nonce,joiner_nonce,reason,grant_hash FROM invites WHERE {column}=?"
            ),
            [value],
            |r| {
                Ok(Row {
                    join_id: r.get(0)?,
                    epoch: r.get(1)?,
                    expires_at_ms: r.get(2)?,
                    device_id: r.get(3)?,
                    kind: r.get(4)?,
                    name: r.get(5)?,
                    commit: r.get(6)?,
                    status: r.get(7)?,
                    admin_id: r.get(8)?,
                    admin_nonce: r.get(9)?,
                    joiner_nonce: r.get(10)?,
                    reason: r.get(11)?,
                    grant_hash: r.get(12)?,
                })
            },
        )
        .optional()?)
}

fn load_state(conn: &Connection) -> Result<NetworkState> {
    let encoded: String = conn
        .query_row("SELECT bundle FROM networks LIMIT 1", [], |r| r.get(0))
        .optional()?
        .context("personal network is not installed")?;
    Ok(serde_json::from_str(&encoded)?)
}
fn save_state(conn: &Connection, state: &NetworkState) -> Result<()> {
    crate::store::install_trust_bundle_in(conn, &state.genesis.network_id, state)?;
    Ok(())
}
fn has_network_in(conn: &Connection) -> Result<bool> {
    Ok(conn.query_row("SELECT COUNT(*) FROM networks", [], |r| r.get::<_, i64>(0))? > 0)
}
fn verify_bootstrap_in(conn: &Connection, capability: &str) -> Result<()> {
    super::validate_hash(capability)?;
    ensure!(!has_network_in(conn)?, "network already installed");
    let expected: String = conn
        .query_row(
            "SELECT value FROM identity_meta WHERE key='bootstrap_capability_hash'",
            [],
            |r| r.get(0),
        )
        .context("bootstrap capability unavailable")?;
    ensure!(
        !expected.starts_with("consumed:") && crypto::digest(capability.as_bytes()) == expected,
        "invalid bootstrap capability"
    );
    Ok(())
}
fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(i64::MAX as u128) as i64
}
pub(crate) fn random_hex(bytes: usize) -> String {
    let mut data = vec![0_u8; bytes];
    OsRng.fill_bytes(&mut data);
    hex::encode(data)
}

#[cfg(test)]
#[path = "local_tests.rs"]
mod tests;
