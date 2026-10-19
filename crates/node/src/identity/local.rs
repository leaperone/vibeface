//! Private, local state for identity handshakes and network membership.
//!
//! One-time capability secrets stay on the client; this database stores their
//! hashes and signed pairing receipts, never a recovery root or client private key.

use super::{AuthorizationView, IdentityKind, Member, NetworkState};
use crate::crypto;
use crate::model::NodeRole;
use crate::store::Store;
use anyhow::{Context, Result, bail, ensure};
use rand::{RngCore, rngs::OsRng};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use serde::{Deserialize, Serialize};
use std::time::{SystemTime, UNIX_EPOCH};

const INVITE_TTL_MS: i64 = 10 * 60_000;

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
    pub epoch: u64,
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

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PendingPair {
    pub request_id: String,
    pub review: PairReview,
    pub new_subject_signature: String,
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

    pub fn bootstrap_network(
        &self,
        state: NetworkState,
        capability: &str,
        remote_client_id: &str,
    ) -> Result<AuthorizationView> {
        ensure!(
            state.checkpoint.is_none() && state.events.is_empty(),
            "bootstrap accepts genesis only"
        );
        self.bootstrap_network_inner(state, capability, remote_client_id)
    }

    pub fn bootstrap_recovered_network(
        &self,
        state: NetworkState,
        capability: &str,
        remote_client_id: &str,
    ) -> Result<AuthorizationView> {
        ensure!(
            state.checkpoint.is_some() && state.events.is_empty(),
            "recovery bootstrap requires a signed checkpoint"
        );
        self.bootstrap_network_inner(state, capability, remote_client_id)
    }

    pub fn verify_bootstrap_capability(&self, capability: &str) -> Result<()> {
        let conn = &self.connection;
        verify_bootstrap_in(conn, capability)
    }

    fn bootstrap_network_inner(
        &self,
        state: NetworkState,
        capability: &str,
        remote_client_id: &str,
    ) -> Result<AuthorizationView> {
        let view = state.project()?;
        ensure!(
            view.authorize_client(remote_client_id)?.manage,
            "remote bootstrap identity must manage"
        );
        self.transaction(|tx|{
            ensure!(!has_network_in(tx)?,"network already installed");
            verify_bootstrap_in(tx,capability)?;
            save_state(tx,&state)?;
            tx.execute("UPDATE identity_meta SET value='consumed:' || value WHERE key='bootstrap_capability_hash'",[])?;
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
        let conn = &self.connection;
        let state = load_state(conn)?;
        state.project()?;
        Ok(state)
    }

    pub fn has_network(&self) -> Result<bool> {
        let conn = &self.connection;
        has_network_in(conn)
    }

    pub fn install_state(&mut self, incoming: NetworkState) -> Result<AuthorizationView> {
        incoming.project()?;
        self.retire_identity_readers(&incoming.genesis.network_id);
        self.transaction(|tx| {
            let next = if has_network_in(tx)? {
                let current = load_state(tx)?;
                let merged = current.merge(&incoming)?;
                if merged.epoch() > current.epoch() {
                    tx.execute_batch(
                        "DELETE FROM identity_pair_reviews; DELETE FROM identity_pair_requests;
                         DELETE FROM identity_invites; DELETE FROM identity_requests;",
                    )?;
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

    pub fn create_invite(&self, request: InviteRequest) -> Result<InvitePayload> {
        super::validate_id(&request.capability)?;
        let mut roles = request.node_roles;
        roles.sort_by_key(super::role_key);
        roles.dedup();
        let payload = InvitePayload {
            version: super::IDENTITY_VERSION,
            invite_id: random_id(),
            request_id: request.request_id,
            network_id: request.network_id,
            inviter_subject_id: request.inviter_subject_id,
            capability_hash: crypto::digest(request.capability.as_bytes()),
            new_kind: request.new_kind,
            node_roles: roles,
            manage: request.manage,
            epoch: request.epoch,
            expires_at_ms: request.expires_at_ms.unwrap_or(now_ms() + INVITE_TTL_MS),
            nonce: random_hex(32),
        };
        let view = self.public_state()?.project()?;
        validate_invite(&payload, &view)?;
        Ok(payload)
    }

    pub fn commit_invite(&self, envelope: InviteEnvelope) -> Result<Receipt> {
        self.transaction(|tx|{
            let payload=&envelope.payload;
            let view=load_state(tx)?.project()?;
            let inviter=validate_invite(payload,&view)?;
            crypto::verify_payload(&inviter.public_key,"identity-invite",&payload.signing_bytes()?,&envelope.inviter_signature)?;
            if let Some((old,signature))=tx.query_row("SELECT payload_hash,inviter_signature FROM identity_invites WHERE invite_id=?",[&payload.invite_id],|r|Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?))).optional()? {
                ensure!(old==payload.hash()?&&signature==envelope.inviter_signature,"invite id reused with another payload");
                return Ok(invite_receipt(payload));
            }
            let pending:i64=tx.query_row("SELECT COUNT(*) FROM identity_invites WHERE used=0 AND expires_at_ms>?",[now_ms()],|r|r.get(0))?;
            ensure!(pending<32,"too many pending invitations");
            tx.execute("INSERT INTO identity_invites(invite_id,request_id,network_id,inviter_subject_id,payload,payload_hash,capability_hash,inviter_signature,epoch,expires_at_ms) VALUES(?,?,?,?,?,?,?,?,?,?)",params![payload.invite_id,payload.request_id,payload.network_id,payload.inviter_subject_id,serde_json::to_string(payload)?,payload.hash()?,payload.capability_hash,envelope.inviter_signature,payload.epoch,payload.expires_at_ms])?;
            Ok(invite_receipt(payload))
        })
    }

    pub fn pair_request(&self, request: PairRequest) -> Result<PairReview> {
        validate_text(&request.request_id, 128)?;
        super::validate_id(&request.capability)?;
        super::validate_id(&request.nonce)?;
        request.member.validate()?;
        ensure!(
            !request.member.manage,
            "management clients require a root checkpoint"
        );
        crypto::verify_payload(
            &request.member.public_key,
            "identity-pair",
            &request.signing_bytes()?,
            &request.proof,
        )?;
        let proposal_hash = crypto::digest(&request.signing_bytes()?);
        self.transaction(|tx|{
            let view=load_state(tx)?.project()?;
            let (encoded,hash,used):(String,String,i64)=tx.query_row("SELECT payload,capability_hash,used FROM identity_invites WHERE invite_id=?",[&request.invite_id],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?)))?;
            ensure!(crypto::digest(request.capability.as_bytes())==hash,"invalid invite capability");
            let payload:InvitePayload=serde_json::from_str(&encoded)?;
            ensure!(payload.capability_hash==hash,"invitation capability mismatch");
            if let Some((id,encoded))=tx.query_row("SELECT request_id,review FROM identity_pair_requests WHERE invite_id=?",[&request.invite_id],|r|Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?))).optional()? {
                let review:PairReview=serde_json::from_str(&encoded)?;
                ensure!(id==request.request_id&&review.new_member==request.member&&review.proposal_hash==proposal_hash,"invitation is already bound to another pairing request");
                ensure!(view.epoch==review.epoch,"pair review epoch is stale");
                return Ok(review);
            }
            validate_invite(&payload,&view)?;
            ensure!(used==0,"invitation has already been used");
            ensure!(request.member.kind==payload.new_kind&&request.member.node_roles==payload.node_roles&&request.member.manage==payload.manage,"member does not match invitation");
            ensure!(!view.members.iter().any(|m|m.subject_id==request.member.subject_id)&&!view.revoked.contains(&request.member.subject_id),"identity is already enrolled or revoked in this epoch");
            let pending:i64=tx.query_row("SELECT COUNT(*) FROM identity_pair_requests",[],|r|r.get(0))?;
            ensure!(pending<super::MAX_MEMBERS as i64,"too many pairing records");
            let mut stmt=tx.prepare("SELECT review FROM identity_pair_requests WHERE subject_id=?")?;
            let records=stmt.query_map([&request.member.subject_id],|r|r.get::<_,String>(0))?;
            for record in records {
                let previous:PairReview=serde_json::from_str(&record?)?;
                ensure!(previous.epoch!=view.epoch||previous.expires_at_ms<=now_ms(),"identity already has a pending pairing request");
            }
            let review=PairReview{version:super::IDENTITY_VERSION,invite_id:request.invite_id.clone(),network_id:payload.network_id,inviter_subject_id:payload.inviter_subject_id,new_member:request.member.clone(),proposal_hash,parents:view.heads,epoch:payload.epoch,expires_at_ms:payload.expires_at_ms};
            tx.execute("INSERT INTO identity_pair_requests(request_id,invite_id,subject_id,review,review_hash,new_subject_signature) VALUES(?,?,?,?,?,'')",params![request.request_id,review.invite_id,review.new_member.subject_id,serde_json::to_string(&review)?,review.hash()?])?;
            Ok(review)
        })
    }

    pub fn accept_pair(
        &self,
        review: PairReview,
        new_subject_signature: String,
    ) -> Result<Receipt> {
        self.transaction(|tx|{
            let view=load_state(tx)?.project()?;
            validate_review(&review,&view)?;
            let (request_id,encoded,hash,old):(String,String,String,String)=tx.query_row("SELECT request_id,review,review_hash,new_subject_signature FROM identity_pair_requests WHERE invite_id=?",[&review.invite_id],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?)))?;
            ensure!(encoded==serde_json::to_string(&review)?&&hash==review.hash()?,"pair review differs from pending request");
            crypto::verify_payload(&review.new_member.public_key,"identity-pair-review",&review.signing_bytes()?,&new_subject_signature)?;
            ensure!(old.is_empty()||old==new_subject_signature,"pairing already accepted with a different signature");
            tx.execute("UPDATE identity_pair_requests SET new_subject_signature=? WHERE invite_id=?",params![new_subject_signature,review.invite_id])?;
            Ok(Receipt{request_id,subject_id:review.new_member.subject_id.clone(),kind:"pair-accepted".into()})
        })
    }

    pub fn lookup_pair(&self, request_id: &str) -> Result<PairReview> {
        let conn = &self.connection;
        let encoded: String = conn.query_row(
            "SELECT review FROM identity_pair_requests WHERE request_id=?",
            [request_id],
            |r| r.get(0),
        )?;
        Ok(serde_json::from_str(&encoded)?)
    }

    pub fn pending_pairs(&self, inviter_id: &str) -> Result<Vec<PendingPair>> {
        let conn = &self.connection;
        let view = load_state(conn)?.project()?;
        ensure!(
            view.authorize_client(inviter_id)?.manage,
            "management permission required"
        );
        let mut stmt=conn.prepare("SELECT p.request_id,p.review,p.new_subject_signature FROM identity_pair_requests p WHERE p.new_subject_signature<>'' AND NOT EXISTS(SELECT 1 FROM identity_requests r WHERE r.request_id='pair:' || p.invite_id)")?;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
            ))
        })?;
        let mut result = vec![];
        for row in rows {
            let (request_id, encoded, new_subject_signature) = row?;
            let review: PairReview = serde_json::from_str(&encoded)?;
            if review.inviter_subject_id == inviter_id
                && review.epoch == view.epoch
                && review.expires_at_ms > now_ms()
            {
                result.push(PendingPair {
                    request_id,
                    review,
                    new_subject_signature,
                });
            }
        }
        Ok(result)
    }

    pub fn confirm_pair(&self, confirm: PairConfirm) -> Result<GrantProposal> {
        self.transaction(|tx|{
            let review=&confirm.review;
            let view=load_state(tx)?.project()?;
            let inviter=validate_review(review,&view)?;
            let (encoded,hash,accepted):(String,String,String)=tx.query_row("SELECT review,review_hash,new_subject_signature FROM identity_pair_requests WHERE invite_id=?",[&review.invite_id],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?)))?;
            ensure!(encoded==serde_json::to_string(review)?&&hash==review.hash()?,"pair review differs from pending request");
            ensure!(!accepted.is_empty()&&accepted==confirm.new_subject_signature,"joining identity has not confirmed this review");
            crypto::verify_payload(&inviter.public_key,"identity-pair-review",&review.signing_bytes()?,&confirm.inviter_signature)?;
            crypto::verify_payload(&review.new_member.public_key,"identity-pair-review",&review.signing_bytes()?,&confirm.new_subject_signature)?;
            if let Some((old,issuer_sig,new_sig))=tx.query_row("SELECT review_hash,inviter_signature,new_subject_signature FROM identity_pair_reviews WHERE invite_id=?",[&review.invite_id],|r|Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?,r.get::<_,String>(2)?))).optional()? {
                ensure!(old==hash&&issuer_sig==confirm.inviter_signature&&new_sig==confirm.new_subject_signature,"pairing is already confirmed with another review");
            } else {
                tx.execute("INSERT INTO identity_pair_reviews(invite_id,review,review_hash,inviter_signature,new_subject_signature,created_at_ms) VALUES(?,?,?,?,?,?)",params![review.invite_id,encoded,hash,confirm.inviter_signature,confirm.new_subject_signature,now_ms()])?;
                tx.execute("UPDATE identity_invites SET used=1 WHERE invite_id=?",[&review.invite_id])?;
            }
            Ok(GrantProposal{review:review.clone(),inviter_signature:confirm.inviter_signature,new_subject_signature:confirm.new_subject_signature})
        })
    }

    pub fn store_signed_grant(&mut self, event: super::GovernanceEvent) -> Result<NetworkState> {
        self.retire_identity_readers(&event.network_id);
        let super::GovernanceAction::Grant { member } = &event.action else {
            bail!("pairing requires a grant")
        };
        let member = member.clone();
        self.transaction(|tx|{
            let mut stmt=tx.prepare("SELECT review,inviter_signature,new_subject_signature FROM identity_pair_reviews")?;
            let rows=stmt.query_map([],|r|Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?,r.get::<_,String>(2)?)))?;
            let mut matching=None;
            for row in rows {
                let (encoded,inviter_signature,new_subject_signature)=row?;
                let review:PairReview=serde_json::from_str(&encoded)?;
                if event.network_id==review.network_id&&event.epoch==review.epoch&&event.issuer==review.inviter_subject_id&&event.parents==review.parents&&member==review.new_member {
                    ensure!(matching.is_none(),"ambiguous confirmed pairing");
                    matching=Some((review,inviter_signature,new_subject_signature));
                }
            }
            let (review,issuer_sig,new_sig)=matching.context("grant does not match a confirmed pairing")?;
            let mut state=load_state(tx)?;
            let view=state.project()?;
            let inviter=validate_review(&review,&view)?;
            crypto::verify_payload(&inviter.public_key,"identity-pair-review",&review.signing_bytes()?,&issuer_sig)?;
            crypto::verify_payload(&review.new_member.public_key,"identity-pair-review",&review.signing_bytes()?,&new_sig)?;
            let receipt_id=format!("pair:{}",review.invite_id);
            let event_hash=event.hash()?;
            if let Some(previous)=tx.query_row("SELECT response FROM identity_requests WHERE request_id=?",[&receipt_id],|r|r.get::<_,String>(0)).optional()? {
                ensure!(previous==event_hash,"pairing already completed with another grant");
                return Ok(state);
            }
            state.append(event)?;
            ensure!(state.project()?.authorize_member(&member.subject_id)?==member,"approved pairing permissions no longer apply");
            save_state(tx,&state)?;
            tx.execute("INSERT INTO identity_requests(request_id,subject_id,kind,response,created_at_ms) VALUES(?,?,'pair-granted',?,?)",params![receipt_id,member.subject_id,event_hash,now_ms()])?;
            Ok(state)
        })
    }

    pub fn pair_status(
        &self,
        invite_id: &str,
        capability: &str,
        subject_id: &str,
    ) -> Result<Option<NetworkState>> {
        super::validate_id(capability)?;
        let conn = &self.connection;
        let hash: String = conn.query_row(
            "SELECT capability_hash FROM identity_invites WHERE invite_id=?",
            [invite_id],
            |r| r.get(0),
        )?;
        ensure!(
            crypto::digest(capability.as_bytes()) == hash,
            "invalid invite capability"
        );
        let review: Option<String> = conn
            .query_row(
                "SELECT review FROM identity_pair_requests WHERE invite_id=?",
                [invite_id],
                |r| r.get(0),
            )
            .optional()?;
        let Some(encoded) = review else {
            return Ok(None);
        };
        let review: PairReview = serde_json::from_str(&encoded)?;
        ensure!(
            review.new_member.subject_id == subject_id,
            "pair status belongs to another identity"
        );
        let done: i64 = conn.query_row(
            "SELECT COUNT(*) FROM identity_requests WHERE request_id=?",
            [format!("pair:{invite_id}")],
            |r| r.get(0),
        )?;
        if done == 0 {
            return Ok(None);
        };
        let state = load_state(conn)?;
        let view = state.project()?;
        ensure!(
            view.network_id == review.network_id && view.epoch == review.epoch,
            "pairing epoch is stale"
        );
        view.authorize_member(subject_id)?;
        Ok(Some(state))
    }
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
    super::validate_id(capability)?;
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
fn validate_invite(payload: &InvitePayload, view: &AuthorizationView) -> Result<Member> {
    ensure!(
        payload.version == super::IDENTITY_VERSION
            && payload.network_id == view.network_id
            && payload.epoch == view.epoch,
        "invitation network or epoch mismatch"
    );
    validate_text(&payload.invite_id, 64)?;
    validate_text(&payload.request_id, 128)?;
    super::validate_id(&payload.capability_hash)?;
    super::validate_id(&payload.nonce)?;
    ensure!(
        payload.expires_at_ms > now_ms() && payload.expires_at_ms <= now_ms() + INVITE_TTL_MS,
        "invalid invitation expiry"
    );
    let inviter = view.authorize_client(&payload.inviter_subject_id)?;
    ensure!(inviter.manage, "management permission required");
    ensure!(
        !payload.manage,
        "management-client changes require a root checkpoint"
    );
    Member {
        subject_id: inviter.subject_id.clone(),
        public_key: inviter.public_key.clone(),
        kind: payload.new_kind,
        name: "new device".into(),
        node_roles: payload.node_roles.clone(),
        manage: false,
    }
    .validate()?;
    Ok(inviter)
}
fn validate_review(review: &PairReview, view: &AuthorizationView) -> Result<Member> {
    ensure!(
        review.version == super::IDENTITY_VERSION
            && review.network_id == view.network_id
            && review.epoch == view.epoch,
        "pair review network or epoch mismatch"
    );
    ensure!(review.expires_at_ms > now_ms(), "pair review expired");
    review.new_member.validate()?;
    ensure!(
        !review.new_member.manage && !view.revoked.contains(&review.new_member.subject_id),
        "pairing cannot grant management or revoked identity"
    );
    let inviter = view.authorize_client(&review.inviter_subject_id)?;
    ensure!(inviter.manage, "management permission required");
    Ok(inviter)
}
fn invite_receipt(payload: &InvitePayload) -> Receipt {
    Receipt {
        request_id: payload.request_id.clone(),
        subject_id: payload.inviter_subject_id.clone(),
        kind: "invite".into(),
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
#[path = "local_tests.rs"]
mod tests;
