//! Private, local state for identity handshakes and network membership.
//!
//! This module deliberately stores only hashes for one-time capabilities and
//! session tokens.  It never stores a root private key or signs a governance
//! grant on behalf of a browser.

use super::{AuthorizationView, ClientPrincipal, IdentityKind, Member, NetworkState};
use crate::crypto::{self, LocalIdentity};
use crate::model::{validate_role_dependencies, NodeRole};
use anyhow::{bail, ensure, Context, Result};
use rand::{rngs::OsRng, RngCore};
use rusqlite::{params, Connection, OpenFlags, OptionalExtension, TransactionBehavior};
use serde::{Deserialize, Serialize};
use std::{
    fs::{self, OpenOptions},
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
    sync::Mutex,
    time::{SystemTime, UNIX_EPOCH},
};

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS identity_meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS identity_network (network_id TEXT PRIMARY KEY, state TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS identity_challenges (
    challenge_id TEXT PRIMARY KEY, request_id TEXT NOT NULL UNIQUE, network_id TEXT NOT NULL,
    subject_id TEXT NOT NULL, public_key TEXT NOT NULL, target_node_id TEXT NOT NULL,
    origin TEXT NOT NULL, channel_binding TEXT NOT NULL, epoch INTEGER NOT NULL,
    nonce TEXT NOT NULL, expires_at_ms INTEGER NOT NULL, used INTEGER NOT NULL DEFAULT 0
);
CREATE TABLE IF NOT EXISTS identity_sessions (
    token_hash TEXT PRIMARY KEY, network_id TEXT NOT NULL, subject_id TEXT NOT NULL,
    target_node_id TEXT NOT NULL, channel_binding TEXT NOT NULL, epoch INTEGER NOT NULL,
    expires_at_ms INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS identity_requests (
    request_id TEXT PRIMARY KEY, subject_id TEXT NOT NULL, kind TEXT NOT NULL,
    response TEXT NOT NULL, created_at_ms INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS identity_invites (
    invite_id TEXT PRIMARY KEY, request_id TEXT NOT NULL UNIQUE, network_id TEXT NOT NULL,
    inviter_subject_id TEXT NOT NULL, payload TEXT NOT NULL, payload_hash TEXT NOT NULL,
    capability_hash TEXT NOT NULL, inviter_signature TEXT NOT NULL,
    epoch INTEGER NOT NULL, expires_at_ms INTEGER NOT NULL, used INTEGER NOT NULL DEFAULT 0
);
CREATE TABLE IF NOT EXISTS identity_pair_reviews (
    invite_id TEXT PRIMARY KEY, review TEXT NOT NULL, review_hash TEXT NOT NULL,
    inviter_signature TEXT NOT NULL, new_subject_signature TEXT NOT NULL,
    created_at_ms INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS identity_pair_requests (
    request_id TEXT PRIMARY KEY, invite_id TEXT NOT NULL, subject_id TEXT NOT NULL,
    review TEXT NOT NULL, review_hash TEXT NOT NULL
);
PRAGMA user_version = 1;
";

const CHALLENGE_TTL_MS: i64 = 90_000;
const SESSION_TTL_MS: i64 = 30 * 60_000;
const INVITE_TTL_MS: i64 = 10 * 60_000;
const MAX_NAME: usize = 128;

#[derive(Debug)]
pub struct IdentityStore {
    path: PathBuf,
    connection: Mutex<Connection>,
}

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

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ChallengeStart {
    pub request_id: String,
    pub network_id: String,
    pub client_subject_id: String,
    pub client_public_key: String,
    pub target_node_id: String,
    pub origin: String,
    pub channel_binding: String,
    pub epoch: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Challenge {
    pub challenge_id: String,
    pub request_id: String,
    pub network_id: String,
    pub client_subject_id: String,
    pub client_public_key: String,
    pub target_node_id: String,
    pub origin: String,
    pub channel_binding: String,
    pub epoch: u64,
    pub nonce: String,
    pub expires_at_ms: i64,
}

impl Challenge {
    pub fn signing_bytes(&self) -> Result<Vec<u8>> {
        let mut unsigned = self.clone();
        unsigned.nonce = format!("{}:{}", unsigned.challenge_id, unsigned.nonce);
        super::canonical_json(&unsigned)
    }
}

#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ChallengeFinish {
    pub challenge_id: String,
    pub signature: String,
}
impl std::fmt::Debug for ChallengeFinish {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ChallengeFinish")
            .field("challenge_id", &self.challenge_id)
            .field("signature", &"[redacted]")
            .finish()
    }
}

#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Session {
    pub token: String,
    pub network_id: String,
    pub subject_id: String,
    pub target_node_id: String,
    pub channel_binding: String,
    pub epoch: u64,
    pub expires_at_ms: i64,
}
impl std::fmt::Debug for Session {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Session")
            .field("token", &"[redacted]")
            .field("network_id", &self.network_id)
            .field("subject_id", &self.subject_id)
            .field("target_node_id", &self.target_node_id)
            .field("channel_binding", &self.channel_binding)
            .field("epoch", &self.epoch)
            .field("expires_at_ms", &self.expires_at_ms)
            .finish()
    }
}

#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SessionAuth {
    pub token: String,
    pub network_id: String,
    pub subject_id: String,
    pub target_node_id: String,
    pub channel_binding: String,
}
impl std::fmt::Debug for SessionAuth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SessionAuth")
            .field("token", &"[redacted]")
            .field("network_id", &self.network_id)
            .field("subject_id", &self.subject_id)
            .field("target_node_id", &self.target_node_id)
            .field("channel_binding", &self.channel_binding)
            .finish()
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct InvitePayload {
    pub version: u32,
    pub invite_id: String,
    pub request_id: String,
    pub network_id: String,
    pub inviter_subject_id: String,
    pub capability_hash: String,
    pub new_kind: IdentityKind,
    pub node_roles: Vec<NodeRole>,
    pub manage: bool,
    pub epoch: u64,
    pub expires_at_ms: i64,
    pub nonce: String,
}
impl InvitePayload {
    pub fn signing_bytes(&self) -> Result<Vec<u8>> {
        super::canonical_json(self)
    }
    pub fn hash(&self) -> Result<String> {
        Ok(crypto::digest(&self.signing_bytes()?))
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct InviteEnvelope {
    pub payload: InvitePayload,
    pub inviter_signature: String,
}
impl InviteEnvelope {
    pub fn signing_bytes(&self) -> Result<Vec<u8>> {
        self.payload.signing_bytes()
    }
    pub fn hash(&self) -> Result<String> {
        self.payload.hash()
    }
}

#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct InviteRequest {
    pub request_id: String,
    pub network_id: String,
    pub inviter_subject_id: String,
    pub capability: String,
    pub new_kind: IdentityKind,
    pub node_roles: Vec<NodeRole>,
    pub manage: bool,
    pub expires_at_ms: Option<i64>,
}
impl std::fmt::Debug for InviteRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InviteRequest")
            .field("request_id", &self.request_id)
            .field("network_id", &self.network_id)
            .field("inviter_subject_id", &self.inviter_subject_id)
            .field("capability", &"[redacted]")
            .field("new_kind", &self.new_kind)
            .field("node_roles", &self.node_roles)
            .field("manage", &self.manage)
            .field("expires_at_ms", &self.expires_at_ms)
            .finish()
    }
}

#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PairRequest {
    pub request_id: String,
    pub invite_id: String,
    pub capability: String,
    pub member: Member,
    pub nonce: String,
    pub proof: String,
}
impl std::fmt::Debug for PairRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PairRequest")
            .field("request_id", &self.request_id)
            .field("invite_id", &self.invite_id)
            .field("capability", &"[redacted]")
            .field("member", &self.member)
            .field("nonce", &self.nonce)
            .field("proof", &"[redacted]")
            .finish()
    }
}
impl PairRequest {
    pub fn signing_bytes(&self) -> Result<Vec<u8>> {
        super::canonical_json(&(
            self.request_id.clone(),
            self.invite_id.clone(),
            self.capability.clone(),
            &self.member,
            self.nonce.clone(),
        ))
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PairReview {
    pub version: u32,
    pub invite_id: String,
    pub network_id: String,
    pub inviter_subject_id: String,
    pub new_member: Member,
    pub proposal_hash: String,
    pub parents: Vec<String>,
    pub epoch: u64,
    pub expires_at_ms: i64,
}
impl PairReview {
    pub fn signing_bytes(&self) -> Result<Vec<u8>> {
        super::canonical_json(self)
    }
    pub fn hash(&self) -> Result<String> {
        Ok(crypto::digest(&self.signing_bytes()?))
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PairConfirm {
    pub review: PairReview,
    pub inviter_signature: String,
    pub new_subject_signature: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct GrantProposal {
    pub review: PairReview,
    pub inviter_signature: String,
    pub new_subject_signature: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Receipt {
    pub request_id: String,
    pub subject_id: String,
    pub kind: String,
}

impl IdentityStore {
    pub fn open(path: &Path) -> Result<Self> {
        ensure!(
            path != Path::new(":memory:"),
            "identity state must be a private file"
        );
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            let existed = parent.exists();
            fs::create_dir_all(parent)?;
            let meta = fs::symlink_metadata(parent)?;
            ensure!(!meta.file_type().is_symlink() && meta.is_dir(), "identity parent must be a directory");
            if existed {
                ensure!(meta.permissions().mode() & 0o077 == 0, "identity parent must be private");
            } else {
                fs::set_permissions(parent, fs::Permissions::from_mode(0o700))?;
            }
        }
        if let Ok(meta) = fs::symlink_metadata(path) {
            ensure!(
                !meta.file_type().is_symlink(),
                "identity database must not be a symlink"
            );
            ensure!(
                meta.file_type().is_file(),
                "identity database must be a regular file"
            );
            fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
        } else {
            OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(path)?;
        }
        for suffix in ["-wal", "-shm", "-journal"] {
            let sidecar = PathBuf::from(format!("{}{}", path.display(), suffix));
            if let Ok(meta) = fs::symlink_metadata(&sidecar) {
                ensure!(
                    !meta.file_type().is_symlink(),
                    "identity sidecar must not be a symlink"
                );
                fs::set_permissions(&sidecar, fs::Permissions::from_mode(0o600))?;
            }
        }
        let connection = Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NOFOLLOW,
        )
        .context("open private identity database")?;
        connection.busy_timeout(std::time::Duration::from_secs(5))?;
        connection.pragma_update(None, "foreign_keys", true)?;
        connection.pragma_update(None, "journal_mode", "WAL")?;
        connection.execute_batch(SCHEMA)?;
        for suffix in ["-wal", "-shm"] {
            let sidecar = PathBuf::from(format!("{}{}", path.display(), suffix));
            if sidecar.exists() {
                fs::set_permissions(sidecar, fs::Permissions::from_mode(0o600))?;
            }
        }
        Ok(Self {
            path: path.to_path_buf(),
            connection: Mutex::new(connection),
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn bootstrap_capability(&self) -> Result<BootstrapCapability> {
        let mut secret = [0_u8; 32];
        OsRng.fill_bytes(&mut secret);
        let capability = hex::encode(secret);
        let hash = crypto::digest(capability.as_bytes());
        let mut conn = self
            .connection
            .lock()
            .map_err(|_| anyhow::anyhow!("identity store lock poisoned"))?;
        let tx = conn.unchecked_transaction()?;
        ensure!(
            tx.query_row("SELECT COUNT(*) FROM identity_network", [], |r| r
                .get::<_, i64>(0))?
                == 0,
            "network already installed"
        );
        ensure!(
            tx.query_row(
                "SELECT COUNT(*) FROM identity_meta WHERE key='bootstrap_capability_hash'",
                [],
                |r| r.get::<_, i64>(0)
            )? == 0,
            "bootstrap capability already issued"
        );
        tx.execute(
            "INSERT INTO identity_meta(key,value) VALUES('bootstrap_capability_hash',?1)",
            [&hash],
        )?;
        tx.commit()?;
        Ok(BootstrapCapability { capability })
    }

    pub fn bootstrap_network(
        &self,
        state: NetworkState,
        capability: &str,
        remote_client_id: &str,
    ) -> Result<AuthorizationView> {
        ensure!(
            !capability.is_empty() && capability.len() == 64,
            "invalid bootstrap capability"
        );
        let view = state.project()?;
        ensure!(
            state.checkpoint.is_none() && state.events.is_empty(),
            "bootstrap accepts genesis only"
        );
        ensure!(
            view.authorize_client(remote_client_id)?.manage,
            "remote bootstrap identity must manage"
        );
        let mut conn = self
            .connection
            .lock()
            .map_err(|_| anyhow::anyhow!("identity store lock poisoned"))?;
        let tx = conn.unchecked_transaction()?;
        ensure!(
            tx.query_row("SELECT COUNT(*) FROM identity_network", [], |r| r
                .get::<_, i64>(0))?
                == 0,
            "network already installed"
        );
        let expected: String = tx
            .query_row(
                "SELECT value FROM identity_meta WHERE key='bootstrap_capability_hash'",
                [],
                |r| r.get(0),
            )
            .context("bootstrap capability unavailable")?;
        ensure!(!expected.starts_with("consumed:"), "bootstrap capability already consumed");
        ensure!(
            crypto::digest(capability.as_bytes()) == expected,
            "invalid bootstrap capability"
        );
        tx.execute(
            "INSERT INTO identity_network(network_id,state) VALUES(?1,?2)",
            params![state.genesis.network_id, serde_json::to_string(&state)?],
        )?;
        tx.execute("UPDATE identity_meta SET value='consumed:' || value WHERE key='bootstrap_capability_hash'", [])?;
        tx.commit()?;
        Ok(view)
    }

    pub fn init_network(&self, state: NetworkState) -> Result<AuthorizationView> {
        let view = state.project()?;
        let conn = self
            .connection
            .lock()
            .map_err(|_| anyhow::anyhow!("identity store lock poisoned"))?;
        let tx = conn.unchecked_transaction()?;
        ensure!(
            tx.query_row("SELECT COUNT(*) FROM identity_network", [], |r| r
                .get::<_, i64>(0))?
                == 0,
            "network already installed"
        );
        tx.execute(
            "INSERT INTO identity_network(network_id,state) VALUES(?1,?2)",
            params![state.genesis.network_id, serde_json::to_string(&state)?],
        )?;
        tx.commit()?;
        Ok(view)
    }

    pub fn public_state(&self) -> Result<NetworkState> {
        let mut conn = self
            .connection
            .lock()
            .map_err(|_| anyhow::anyhow!("identity store lock poisoned"))?;
        conn.query_row("SELECT state FROM identity_network LIMIT 1", [], |r| {
            r.get::<_, String>(0)
        })
        .optional()?
        .context("personal network is not installed")
        .and_then(|encoded| Ok(serde_json::from_str(&encoded)?))
    }

    pub fn install_state(&self, incoming: NetworkState) -> Result<AuthorizationView> {
        let incoming_view = incoming.project()?;
        let mut conn = self
            .connection
            .lock()
            .map_err(|_| anyhow::anyhow!("identity store lock poisoned"))?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let previous: Option<String> = tx
            .query_row("SELECT state FROM identity_network LIMIT 1", [], |r| {
                r.get(0)
            })
            .optional()?;
        let next = if let Some(encoded) = previous {
            let current: NetworkState = serde_json::from_str(&encoded)?;
            current.merge(&incoming)?
        } else {
            incoming
        };
        tx.execute("INSERT INTO identity_network(network_id,state) VALUES(?1,?2) ON CONFLICT(network_id) DO UPDATE SET state=excluded.state", params![next.genesis.network_id, serde_json::to_string(&next)?])?;
        tx.commit()?;
        Ok(next.project()?)
    }

    pub fn begin_challenge(&self, request: ChallengeStart) -> Result<Challenge> {
        let view = self.public_state()?.project()?;
        ensure!(
            request.network_id == view.network_id && request.epoch == view.epoch,
            "challenge network or epoch mismatch"
        );
        let member = view.authorize_client(&request.client_subject_id)?;
        ensure!(
            member.public_key == request.client_public_key,
            "challenge public key mismatch"
        );
        validate_text(&request.request_id, 128)?;
        validate_text(&request.target_node_id, 128)?;
        validate_text(&request.origin, 256)?;
        validate_text(&request.channel_binding, 512)?;
        let now = now_ms();
        let challenge = Challenge {
            challenge_id: random_id(),
            request_id: request.request_id.clone(),
            network_id: request.network_id,
            client_subject_id: request.client_subject_id,
            client_public_key: request.client_public_key,
            target_node_id: request.target_node_id,
            origin: request.origin,
            channel_binding: request.channel_binding,
            epoch: request.epoch,
            nonce: random_hex(32),
            expires_at_ms: now + CHALLENGE_TTL_MS,
        };
        let conn = self
            .connection
            .lock()
            .map_err(|_| anyhow::anyhow!("identity store lock poisoned"))?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.execute("INSERT INTO identity_challenges(challenge_id,request_id,network_id,subject_id,public_key,target_node_id,origin,channel_binding,epoch,nonce,expires_at_ms) VALUES(?,?,?,?,?,?,?,?,?,?,?)", params![challenge.challenge_id,challenge.request_id,challenge.network_id,challenge.client_subject_id,challenge.client_public_key,challenge.target_node_id,challenge.origin,challenge.channel_binding,challenge.epoch,challenge.nonce,challenge.expires_at_ms])?;
        tx.commit()?;
        Ok(challenge)
    }

    pub fn finish_challenge(&self, finish: ChallengeFinish) -> Result<Session> {
        let conn = self
            .connection
            .lock()
            .map_err(|_| anyhow::anyhow!("identity store lock poisoned"))?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let row = tx.query_row("SELECT request_id,network_id,subject_id,public_key,target_node_id,origin,channel_binding,epoch,nonce,expires_at_ms,used FROM identity_challenges WHERE challenge_id=?", [&finish.challenge_id], |r| Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?,r.get::<_,String>(2)?,r.get::<_,String>(3)?,r.get::<_,String>(4)?,r.get::<_,String>(5)?,r.get::<_,String>(6)?,r.get::<_,u64>(7)?,r.get::<_,String>(8)?,r.get::<_,i64>(9)?,r.get::<_,i64>(10)?)))?;
        ensure!(
            row.10 == 0 && now_ms() <= row.9,
            "challenge expired or already used"
        );
        let challenge = Challenge {
            challenge_id: finish.challenge_id.clone(),
            request_id: row.0,
            network_id: row.1,
            client_subject_id: row.2,
            client_public_key: row.3,
            target_node_id: row.4,
            origin: row.5,
            channel_binding: row.6,
            epoch: row.7,
            nonce: row.8,
            expires_at_ms: row.9,
        };
        crypto::verify_payload(
            &challenge.client_public_key,
            "identity-challenge",
            &challenge.signing_bytes()?,
            &finish.signature,
        )?;
        let view = self.state_in_tx(&tx)?.project()?;
        let member = view.authorize_client(&challenge.client_subject_id)?;
        ensure!(
            view.epoch == challenge.epoch && member.public_key == challenge.client_public_key,
            "challenge authorization changed"
        );
        let token = random_hex(32);
        let session = Session {
            token: token.clone(),
            network_id: challenge.network_id,
            subject_id: challenge.client_subject_id,
            target_node_id: challenge.target_node_id,
            channel_binding: challenge.channel_binding,
            epoch: challenge.epoch,
            expires_at_ms: now_ms() + SESSION_TTL_MS,
        };
        tx.execute(
            "UPDATE identity_challenges SET used=1 WHERE challenge_id=?",
            [&finish.challenge_id],
        )?;
        tx.execute("INSERT INTO identity_sessions(token_hash,network_id,subject_id,target_node_id,channel_binding,epoch,expires_at_ms) VALUES(?,?,?,?,?,?,?)", params![crypto::digest(token.as_bytes()),session.network_id,session.subject_id,session.target_node_id,session.channel_binding,session.epoch,session.expires_at_ms])?;
        tx.commit()?;
        Ok(session)
    }

    pub fn authenticate(&self, request: SessionAuth) -> Result<ClientPrincipal> {
        let token_hash = crypto::digest(request.token.as_bytes());
        let conn = self
            .connection
            .lock()
            .map_err(|_| anyhow::anyhow!("identity store lock poisoned"))?;
        let row = conn.query_row("SELECT network_id,subject_id,target_node_id,channel_binding,epoch,expires_at_ms FROM identity_sessions WHERE token_hash=?", [&token_hash], |r| Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?,r.get::<_,String>(2)?,r.get::<_,String>(3)?,r.get::<_,u64>(4)?,r.get::<_,i64>(5)?)))?;
        ensure!(
            now_ms() <= row.5
                && row.0 == request.network_id
                && row.1 == request.subject_id
                && row.2 == request.target_node_id
                && row.3 == request.channel_binding,
            "invalid session binding"
        );
        let view: NetworkState = conn
            .query_row(
                "SELECT state FROM identity_network WHERE network_id=?",
                [&row.0],
                |r| r.get::<_, String>(0),
            )
            .and_then(|v| {
                serde_json::from_str(&v)
                    .map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))
            })?;
        let principal = view.project()?.principal(&row.1)?;
        ensure!(principal.epoch == row.4, "session epoch is stale");
        Ok(principal)
    }

    pub fn create_invite(&self, request: InviteRequest) -> Result<InvitePayload> {
        let view = self.public_state()?.project()?;
        ensure!(
            request.network_id == view.network_id && request.epoch == view.epoch,
            "invite network or epoch mismatch"
        );
        ensure!(
            view.authorize_client(&request.inviter_subject_id)?.manage,
            "management permission required"
        );
        ensure!(!request.manage, "management clients require a root checkpoint");
        ensure!(request.capability.len() == 64 && request.capability.bytes().all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase()), "invalid invite capability");
        ensure!(
            request.new_kind == IdentityKind::Client || !request.node_roles.is_empty(),
            "node invite needs a role"
        );
        if request.new_kind == IdentityKind::Client {
            ensure!(
                request.node_roles.is_empty(),
                "client invite cannot contain node roles"
            );
        }
        if !request.new_kind.eq(&IdentityKind::Client) {
            ensure!(!request.manage, "node invite cannot manage");
        }
        let cap_hash = crypto::digest(request.capability.as_bytes());
        let payload = InvitePayload {
            version: super::IDENTITY_VERSION,
            invite_id: random_id(),
            request_id: request.request_id,
            network_id: request.network_id,
            inviter_subject_id: request.inviter_subject_id,
            capability_hash: cap_hash.clone(),
            new_kind: request.new_kind,
            node_roles: request.node_roles,
            manage: request.manage,
            epoch: request.epoch,
            expires_at_ms: request.expires_at_ms.unwrap_or(now_ms() + INVITE_TTL_MS),
            nonce: random_hex(32),
        };
        ensure!(
            payload.expires_at_ms > now_ms() && payload.expires_at_ms <= now_ms() + INVITE_TTL_MS,
            "invalid invite expiry"
        );
        Ok(payload)
    }

    pub fn commit_invite(&self, envelope: InviteEnvelope) -> Result<Receipt> {
        let payload = &envelope.payload;
        ensure!(payload.version == super::IDENTITY_VERSION, "unsupported invite version");
        ensure!(payload.expires_at_ms > now_ms() && payload.expires_at_ms <= now_ms() + INVITE_TTL_MS, "invite expired or too long");
        ensure!(payload.capability_hash.len() == 64 && payload.capability_hash.bytes().all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase()), "invalid invite capability hash");
        ensure!(!payload.manage, "management clients require a root checkpoint");
        validate_role_dependencies(&payload.node_roles)?;
        ensure!(payload.new_kind == IdentityKind::Client || !payload.node_roles.is_empty(), "node invite needs a role");
        ensure!(payload.new_kind == IdentityKind::Node || payload.node_roles.is_empty(), "client invite cannot contain node roles");
        let view = self.public_state()?.project()?;
        ensure!(
            view.network_id == payload.network_id && view.epoch == payload.epoch,
            "invite epoch is stale"
        );
        let inviter = view.authorize_client(&payload.inviter_subject_id)?;
        ensure!(inviter.manage, "management permission required");
        crypto::verify_payload(
            &inviter.public_key,
            "identity-invite",
            &payload.signing_bytes()?,
            &envelope.inviter_signature,
        )?;
        let conn = self
            .connection
            .lock()
            .map_err(|_| anyhow::anyhow!("identity store lock poisoned"))?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        if let Some(old) = tx
            .query_row(
                "SELECT payload_hash FROM identity_invites WHERE invite_id=?",
                [&payload.invite_id],
                |r| r.get::<_, String>(0),
            )
            .optional()?
        {
            ensure!(
                old == payload.hash()?,
                "invite id reused with another payload"
            );
            return Ok(Receipt {
                request_id: payload.request_id.clone(),
                subject_id: payload.inviter_subject_id.clone(),
                kind: "invite".into(),
            });
        }
        tx.execute("INSERT INTO identity_invites(invite_id,request_id,network_id,inviter_subject_id,payload,payload_hash,capability_hash,inviter_signature,epoch,expires_at_ms) VALUES(?,?,?,?,?,?,?,?,?,?)", params![payload.invite_id,payload.request_id,payload.network_id,payload.inviter_subject_id,serde_json::to_string(payload)?,payload.hash()?,payload.capability_hash,envelope.inviter_signature,payload.epoch,payload.expires_at_ms])?;
        tx.commit()?;
        Ok(Receipt {
            request_id: payload.request_id.clone(),
            subject_id: payload.inviter_subject_id.clone(),
            kind: "invite".into(),
        })
    }

    pub fn pair_request(&self, request: PairRequest) -> Result<PairReview> {
        let view = self.public_state()?.project()?;
        ensure!(request.capability.len() == 64 && request.capability.bytes().all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase()), "invalid invite capability");
        let row = self.connection.lock().map_err(|_| anyhow::anyhow!("identity store lock poisoned"))?.query_row("SELECT payload,capability_hash,inviter_subject_id,network_id,epoch,expires_at_ms,used FROM identity_invites WHERE invite_id=?", [&request.invite_id], |r| Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?,r.get::<_,String>(2)?,r.get::<_,String>(3)?,r.get::<_,u64>(4)?,r.get::<_,i64>(5)?,r.get::<_,i64>(6)?)))?;
        ensure!(
            row.6 == 0 && now_ms() <= row.5 && crypto::digest(request.capability.as_bytes()) == row.1,
            "invite is expired, used, or capability is wrong"
        );
        let payload: InvitePayload = serde_json::from_str(&row.0)?;
        ensure!(
            view.network_id == row.3 && view.epoch == row.4,
            "invite epoch is stale"
        );
        let inviter = view.authorize_client(&row.2)?;
        ensure!(inviter.manage, "inviter no longer manages");
        request.member.validate()?;
        ensure!(
            request.member.kind == payload.new_kind
                && request.member.node_roles == payload.node_roles
                && request.member.manage == payload.manage,
            "member does not match invite"
        );
        ensure!(
            crypto::fingerprint(&request.member.public_key)? == request.member.subject_id,
            "member fingerprint mismatch"
        );
        crypto::verify_payload(
            &request.member.public_key,
            "identity-pair",
            &request.signing_bytes()?,
            &request.proof,
        )?;
        let review = PairReview {
            version: super::IDENTITY_VERSION,
            invite_id: request.invite_id,
            network_id: payload.network_id,
            inviter_subject_id: payload.inviter_subject_id,
            new_member: request.member,
            proposal_hash: crypto::digest(&super::canonical_json(&payload)?),
            parents: view.heads.clone(),
            epoch: payload.epoch,
            expires_at_ms: payload.expires_at_ms,
        };
        let mut conn = self.connection.lock().map_err(|_| anyhow::anyhow!("identity store lock poisoned"))?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        if let Some((subject, encoded)) = tx.query_row("SELECT subject_id,review FROM identity_pair_requests WHERE request_id=?", [&request.request_id], |r| Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?))).optional()? {
            ensure!(subject == review.new_member.subject_id, "request id reused by another identity");
            return Ok(serde_json::from_str(&encoded)?);
        }
        let review_hash = review.hash()?;
        tx.execute("INSERT INTO identity_pair_requests(request_id,invite_id,subject_id,review,review_hash) VALUES(?,?,?,?,?)", params![request.request_id,review.invite_id,review.new_member.subject_id,serde_json::to_string(&review)?,review_hash])?;
        tx.commit()?;
        Ok(review)
    }

    pub fn confirm_pair(&self, confirm: PairConfirm) -> Result<GrantProposal> {
        let review = &confirm.review;
        ensure!(review.expires_at_ms > now_ms(), "pair review expired");
        let view = self.public_state()?.project()?;
        ensure!(
            view.network_id == review.network_id && view.epoch == review.epoch,
            "pair review epoch is stale"
        );
        let inviter = view.authorize_client(&review.inviter_subject_id)?;
        ensure!(inviter.manage, "inviter no longer manages");
        crypto::verify_payload(
            &inviter.public_key,
            "identity-pair-review",
            &review.signing_bytes()?,
            &confirm.inviter_signature,
        )?;
        crypto::verify_payload(
            &review.new_member.public_key,
            "identity-pair-review",
            &review.signing_bytes()?,
            &confirm.new_subject_signature,
        )?;
        let conn = self
            .connection
            .lock()
            .map_err(|_| anyhow::anyhow!("identity store lock poisoned"))?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.execute("INSERT INTO identity_pair_reviews(invite_id,review,review_hash,inviter_signature,new_subject_signature,created_at_ms) VALUES(?,?,?,?,?,?) ON CONFLICT(invite_id) DO UPDATE SET review=excluded.review,review_hash=excluded.review_hash,inviter_signature=excluded.inviter_signature,new_subject_signature=excluded.new_subject_signature", params![review.invite_id,serde_json::to_string(review)?,review.hash()?,confirm.inviter_signature,confirm.new_subject_signature,now_ms()])?;
        tx.execute(
            "UPDATE identity_invites SET used=1 WHERE invite_id=?",
            [&review.invite_id],
        )?;
        tx.commit()?;
        Ok(GrantProposal {
            review: review.clone(),
            inviter_signature: confirm.inviter_signature,
            new_subject_signature: confirm.new_subject_signature,
        })
    }

    fn state_in_tx(&self, tx: &rusqlite::Transaction<'_>) -> Result<NetworkState> {
        let encoded: String =
            tx.query_row("SELECT state FROM identity_network LIMIT 1", [], |r| {
                r.get(0)
            })?;
        Ok(serde_json::from_str(&encoded)?)
    }
}

fn validate_text(value: &str, max: usize) -> Result<()> {
    ensure!(
        !value.is_empty() && value.len() <= max && !value.chars().any(char::is_control),
        "invalid text"
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
fn random_hex(bytes: usize) -> String {
    let mut data = vec![0_u8; bytes];
    OsRng.fill_bytes(&mut data);
    hex::encode(data)
}
fn random_id() -> String {
    random_hex(16)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    fn temp() -> (PathBuf, IdentityStore) {
        let root = std::env::temp_dir().join(format!("vibeface-identity-{}", random_id()));
        fs::create_dir_all(&root).unwrap();
        let db = root.join("identity.sqlite3");
        let store = IdentityStore::open(&db).unwrap();
        (root, store)
    }

    #[test]
    fn bootstrap_is_one_time_and_symlink_is_rejected() -> Result<()> {
        let (root, store) = temp();
        let cap = store.bootstrap_capability()?;
        assert!(store.bootstrap_capability().is_err());
        assert_eq!(
            fs::metadata(store.path())?.permissions().mode() & 0o777,
            0o600
        );
        let link = root.join("link.sqlite3");
        symlink(store.path(), &link)?;
        assert!(IdentityStore::open(&link).is_err());
        Ok(())
    }
}
