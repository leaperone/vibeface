use crate::model::*;
use anyhow::{Context as _, Result, ensure};
use ed25519_dalek::{Signature, Signer, SigningKey, VerifyingKey};
use rand::{RngCore, rngs::OsRng};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

const EVENT_DOMAIN: &[u8] = b"vibeface:event:v1:";

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LocalIdentity {
    pub secret_key_hex: String,
}
impl std::fmt::Debug for LocalIdentity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("LocalIdentity([redacted])")
    }
}
impl LocalIdentity {
    pub fn generate() -> Self {
        Self {
            secret_key_hex: hex::encode(SigningKey::generate(&mut OsRng).to_bytes()),
        }
    }
    fn key(&self) -> Result<SigningKey> {
        Ok(SigningKey::from_bytes(&decode32(&self.secret_key_hex)?))
    }
    pub fn public_key_hex(&self) -> Result<String> {
        Ok(hex::encode(self.key()?.verifying_key().as_bytes()))
    }
    pub fn peer_id(&self) -> Result<PeerId> {
        fingerprint(&self.public_key_hex()?)
    }
    pub fn network_id(&self) -> Result<NetworkId> {
        self.peer_id()
    }
    pub fn certificate(&self, roles: Vec<NodeRole>) -> Result<NodeCertificate> {
        Ok(NodeCertificate {
            peer_id: self.peer_id()?,
            public_key: self.public_key_hex()?,
            roles,
            revoked: false,
        })
    }
    pub fn sign_payload(&self, domain: &str, payload: &[u8]) -> Result<String> {
        vibeface_secure::Identity::from_secret_hex(&self.secret_key_hex)
            .and_then(|identity| identity.sign(domain, payload))
            .map_err(anyhow::Error::msg)
    }
    pub fn sign_event(&self, mut event: Event) -> Result<SignedEvent> {
        ensure!(
            event.author == self.peer_id()?,
            "event author does not match local identity"
        );
        event.parents.sort();
        event.parents.dedup();
        event.blobs.sort();
        event.blobs.dedup();
        validate_event_shape(&event)?;
        let canonical_body = serde_json::to_string(&event)?;
        ensure!(canonical_body.len() <= MAX_EVENT_BYTES, "event too large");
        let hash = digest(canonical_body.as_bytes());
        let signature = hex::encode(
            self.key()?
                .sign(&domain_message(EVENT_DOMAIN, &hash))
                .to_bytes(),
        );
        Ok(SignedEvent {
            hash,
            canonical_body,
            signature,
        })
    }
    #[cfg(test)]
    fn fixture_root(&self) -> Self {
        Self {
            secret_key_hex: digest(
                format!("vibeface:network-fixture-root:v1:{}", self.secret_key_hex).as_bytes(),
            ),
        }
    }
    #[cfg(test)]
    pub fn fixture_network_id(&self) -> Result<String> {
        self.fixture_root().network_id()
    }
    #[cfg(test)]
    pub fn sign_network_fixture(
        &self,
        revision: u64,
        nodes: Vec<NodeCertificate>,
    ) -> Result<crate::identity::NetworkState> {
        use crate::identity::{
            Genesis, HistoricalNode, IdentityKind, Member, NetworkState, RootCheckpoint,
        };

        let mut admin_seed = b"vibeface:network-fixture-admin:v1:".to_vec();
        admin_seed.extend_from_slice(self.secret_key_hex.as_bytes());
        let admin_secret = Sha256::digest(admin_seed);
        let admin = LocalIdentity {
            secret_key_hex: hex::encode(admin_secret),
        };
        let admin_member = Member::client(&admin, "fixture-admin", true)?;
        let root = self.fixture_root();
        let genesis = Genesis::create(&root, vec![admin_member.clone()])?;
        let history = nodes
            .iter()
            .map(|node| HistoricalNode {
                subject_id: node.peer_id.clone(),
                public_key: node.public_key.clone(),
            })
            .collect();
        let mut members = vec![admin_member];
        for certificate in nodes.into_iter().filter(|certificate| !certificate.revoked) {
            let peer_id = certificate.peer_id;
            let public_key = certificate.public_key;
            let mut roles = certificate.roles;
            if roles
                .iter()
                .any(|role| matches!(role, NodeRole::Gateway | NodeRole::Scheduler))
                && !roles.contains(&NodeRole::Replica)
            {
                roles.push(NodeRole::Replica);
            }
            roles.sort_by_key(node_role_key);
            let member = Member {
                subject_id: peer_id.clone(),
                public_key,
                kind: IdentityKind::Node,
                name: format!("node-{peer_id}"),
                node_roles: roles,
                manage: false,
            };
            member.validate()?;
            members.push(member);
        }
        let mut state = NetworkState::new(genesis)?;
        let epoch = revision
            .checked_add(1)
            .context("network fixture revision overflow")?;
        state.recover(RootCheckpoint::create_with_history(
            &root, epoch, members, history,
        )?)?;
        Ok(state)
    }
}
impl vibeface_secure::membership::MembershipSigner for LocalIdentity {
    fn public_key_hex(&self) -> Result<String> {
        LocalIdentity::public_key_hex(self)
    }

    fn sign_payload(&self, domain: &str, payload: &[u8]) -> Result<String> {
        LocalIdentity::sign_payload(self, domain, payload)
    }
}

pub fn random_hex(bytes: usize) -> String {
    let mut data = vec![0_u8; bytes];
    OsRng.fill_bytes(&mut data);
    hex::encode(data)
}

pub fn digest(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}
pub fn fingerprint(public_key: &str) -> Result<String> {
    Ok(digest(&decode32(public_key)?))
}
fn decode32(value: &str) -> Result<[u8; 32]> {
    let bytes = hex::decode(value).context("invalid hexadecimal key/hash")?;
    bytes
        .try_into()
        .map_err(|_| anyhow::anyhow!("expected 32 bytes"))
}
fn domain_message(domain: &[u8], hash: &str) -> Vec<u8> {
    [domain, hash.as_bytes()].concat()
}
fn verify(public_key: &str, signature: &str, message: &[u8]) -> Result<()> {
    let key = VerifyingKey::from_bytes(&decode32(public_key)?)?;
    let signature = Signature::from_slice(&hex::decode(signature)?)?;
    key.verify_strict(message, &signature)
        .context("invalid signature")
}
#[cfg(test)]
fn node_role_key(role: &NodeRole) -> &'static str {
    match role {
        NodeRole::Gateway => "gateway",
        NodeRole::Relay => "relay",
        NodeRole::Replica => "replica",
        NodeRole::Scheduler => "scheduler",
    }
}
pub fn verify_signed_event(
    network_id: &str,
    event: &SignedEvent,
    public_key: &str,
) -> Result<Event> {
    ensure!(
        event.canonical_body.len() <= MAX_EVENT_BYTES,
        "event too large"
    );
    ensure!(
        digest(event.canonical_body.as_bytes()) == event.hash,
        "event hash mismatch"
    );
    let body: Event = serde_json::from_str(&event.canonical_body)?;
    ensure!(
        serde_json::to_string(&body)? == event.canonical_body,
        "noncanonical event encoding"
    );
    ensure!(
        body.network_id == network_id,
        "event belongs to another network"
    );
    ensure!(
        body.author == fingerprint(public_key)?,
        "author Peer ID mismatch"
    );
    validate_event_shape(&body)?;
    verify(
        public_key,
        &event.signature,
        &domain_message(EVENT_DOMAIN, &event.hash),
    )?;
    Ok(body)
}
pub fn verify_payload(
    public_key: &str,
    domain: &str,
    payload: &[u8],
    signature: &str,
) -> Result<()> {
    vibeface_secure::verify(public_key, domain, payload, signature).map_err(anyhow::Error::msg)
}
pub fn validate_event_shape(event: &Event) -> Result<()> {
    ensure!(event.version == WIRE_VERSION, "unsupported event version");
    ensure!(
        !event.scope_id.is_empty() && event.scope_id.len() <= 512,
        "invalid scope"
    );
    decode32(&event.network_id)?;
    decode32(&event.author)?;
    ensure!(
        event.sequence > 0 && event.sequence <= i64::MAX as u64,
        "invalid sequence"
    );
    ensure!(
        (event.sequence == 1) == event.previous.is_none(),
        "invalid predecessor"
    );
    ensure!(
        event.parents.len() <= 1024 && event.blobs.len() <= 128,
        "too many dependencies"
    );
    ensure!(
        event.parents.windows(2).all(|w| w[0] < w[1]),
        "parents must be sorted and unique"
    );
    ensure!(
        event.blobs.windows(2).all(|w| w[0] < w[1]),
        "blobs must be sorted and unique"
    );
    for hash in event.parents.iter().chain(event.previous.iter()) {
        decode32(hash)?;
    }
    for blob in &event.blobs {
        decode32(&blob.hash)?;
        ensure!(blob.size <= MAX_BLOB_BYTES as u64, "blob too large");
    }
    if let EventBody::ContextCheckpoint { covered_heads, .. } = &event.body {
        for hash in covered_heads {
            ensure!(
                event.parents.contains(hash),
                "checkpoint head must be a parent"
            );
        }
    }
    if let EventBody::AgentMessage { message } = &event.body {
        vibeface_protocol::validate_id(&message.message_id).map_err(anyhow::Error::msg)?;
        decode32(&message.target_node)?;
        decode32(&message.origin_trigger)?;
        ensure!(
            event.parents.contains(&message.origin_trigger),
            "agent origin must be a parent"
        );
        if message.kind == AgentMessageKind::Task {
            ensure!(
                message.in_reply_to.is_none() && !message.text.trim().is_empty(),
                "agent task requires text and cannot reply to another task"
            );
        } else {
            let reference = message
                .in_reply_to
                .as_ref()
                .context("agent reply has no task reference")?;
            decode32(reference)?;
            ensure!(
                event.parents.contains(reference),
                "agent task reference must be a parent"
            );
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {

    use super::*;
    #[test]
    fn signatures_pin_identity_and_original_bytes() -> Result<()> {
        let owner = LocalIdentity::generate();
        let revoked = LocalIdentity::generate();
        let mut revoked_certificate = revoked.certificate(vec![NodeRole::Relay])?;
        revoked_certificate.revoked = true;
        let state = owner.sign_network_fixture(
            1,
            vec![
                owner.certificate(vec![NodeRole::Gateway])?,
                revoked_certificate,
            ],
        )?;
        let view = state.project()?;
        assert_eq!(view.network_id, owner.fixture_network_id()?);
        assert!(
            view.authorize_node(&owner.peer_id()?, Some(NodeRole::Replica))
                .is_ok()
        );
        assert!(view.authorize_node(&revoked.peer_id()?, None).is_err());
        let all = owner.sign_network_fixture(
            1,
            vec![owner.certificate(vec![
                NodeRole::Scheduler,
                NodeRole::Relay,
                NodeRole::Gateway,
            ])?],
        )?;
        assert!(
            all.project()?
                .authorize_node(&owner.peer_id()?, Some(NodeRole::Scheduler))
                .is_ok()
        );
        let later = owner.sign_network_fixture(2, vec![])?;
        assert_eq!(state.genesis, later.genesis);
        let signed = owner.sign_payload("peer-request", b"challenge")?;
        verify_payload(
            &owner.public_key_hex()?,
            "peer-request",
            b"challenge",
            &signed,
        )?;
        assert!(
            verify_payload(
                &owner.public_key_hex()?,
                "peer-response",
                b"challenge",
                &signed
            )
            .is_err()
        );
        assert!(owner.sign_payload("bad:domain", b"challenge").is_err());
        let body = Event {
            version: 1,
            network_id: owner.fixture_network_id()?,
            scope_id: "c".into(),
            author: owner.peer_id()?,
            sequence: 1,
            previous: None,
            parents: vec![],
            created_at_ms: 0,
            body: EventBody::ConversationCreated {
                title: "test".into(),
            },
            blobs: vec![],
        };
        let event = owner.sign_event(body)?;
        verify_signed_event(&view.network_id, &event, &owner.public_key_hex()?)?;
        let mut bad = event.clone();
        bad.canonical_body.push(' ');
        assert!(verify_signed_event(&view.network_id, &bad, &owner.public_key_hex()?).is_err());
        assert!(verify_signed_event(&"0".repeat(64), &event, &owner.public_key_hex()?).is_err());
        let other = LocalIdentity::generate();
        assert!(verify_signed_event(&view.network_id, &event, &other.public_key_hex()?).is_err());
        Ok(())
    }

    #[test]
    fn agent_messages_bind_valid_ids_and_causal_references() -> Result<()> {
        let identity = LocalIdentity::generate();
        let origin = digest(b"origin");
        let task = digest(b"task");
        let mut event = Event {
            version: WIRE_VERSION,
            network_id: identity.fixture_network_id()?,
            scope_id: "conversation".into(),
            author: identity.peer_id()?,
            sequence: 1,
            previous: None,
            parents: vec![origin.clone()],
            created_at_ms: 0,
            body: EventBody::AgentMessage {
                message: AgentMessage {
                    message_id: "work-1".into(),
                    target_node: identity.peer_id()?,
                    origin_trigger: origin,
                    in_reply_to: None,
                    kind: AgentMessageKind::Task,
                    text: "inspect the project".into(),
                },
            },
            blobs: vec![],
        };
        let signed = identity.sign_event(event.clone())?;
        verify_signed_event(&event.network_id, &signed, &identity.public_key_hex()?)?;
        let EventBody::AgentMessage { message } = &mut event.body else {
            unreachable!()
        };
        message.kind = AgentMessageKind::Completed;
        assert!(identity.sign_event(event.clone()).is_err());
        let EventBody::AgentMessage { message } = &mut event.body else {
            unreachable!()
        };
        message.in_reply_to = Some(task.clone());
        assert!(identity.sign_event(event.clone()).is_err());
        event.parents.push(task);
        assert!(identity.sign_event(event.clone()).is_ok());
        let EventBody::AgentMessage { message } = &mut event.body else {
            unreachable!()
        };
        message.target_node = "not-a-device-key".into();
        assert!(identity.sign_event(event).is_err());
        Ok(())
    }
}
