use crate::model::*;
use anyhow::{Context as _, Result, ensure};
use ed25519_dalek::{Signature, Signer, SigningKey, VerifyingKey};
use rand::rngs::OsRng;
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
    /// The device's Ed25519 public key, which is also its `DeviceId`.
    pub fn device_id(&self) -> Result<DeviceId> {
        Ok(hex::encode(self.key()?.verifying_key().as_bytes()))
    }
    /// A Noise handshake whose per-session static key this device signs.
    pub fn handshake(
        &self,
        initiator: bool,
        expected_device_id: Option<String>,
        context: &[u8],
    ) -> std::result::Result<vibeface_secure::Handshake, String> {
        let identity = vibeface_secure::Identity::from_secret_hex(&self.secret_key_hex)?;
        vibeface_secure::Handshake::signed(initiator, &identity, expected_device_id, context)
    }
    pub fn sign_payload(&self, domain: &str, payload: &[u8]) -> Result<String> {
        let message = payload_message(domain, payload)?;
        Ok(hex::encode(self.key()?.sign(&message).to_bytes()))
    }
    pub fn sign_event(&self, mut event: Event) -> Result<SignedEvent> {
        ensure!(
            event.author == self.device_id()?,
            "event author does not match local identity"
        );
        event.deps.sort();
        event.deps.dedup();
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
        self.fixture_root().device_id()
    }
    /// A recovered network whose members are a fixture admin client and `nodes`.
    #[cfg(test)]
    pub fn sign_network_fixture(
        &self,
        revision: u64,
        nodes: Vec<DeviceId>,
    ) -> Result<crate::identity::NetworkState> {
        use crate::identity::{Genesis, IdentityKind, Member, NetworkState, RootCheckpoint};

        let mut admin_seed = b"vibeface:network-fixture-admin:v1:".to_vec();
        admin_seed.extend_from_slice(self.secret_key_hex.as_bytes());
        let admin = LocalIdentity {
            secret_key_hex: hex::encode(Sha256::digest(admin_seed)),
        };
        let admin_member = Member::client(&admin, "fixture-admin", true)?;
        let root = self.fixture_root();
        let genesis = Genesis::create(
            &root,
            admin_member.clone(),
            Member::node(self, "fixture-node")?,
        )?;
        let mut members = vec![admin_member];
        for node in nodes {
            let name = format!("node-{}", &node[..8]);
            members.push(Member::new(&node, IdentityKind::Node, &name, false)?);
        }
        let mut state = NetworkState::new(genesis)?;
        let epoch = revision
            .checked_add(1)
            .context("network fixture revision overflow")?;
        state.recover(RootCheckpoint::create(&root, epoch, members)?)?;
        Ok(state)
    }
}
impl vibeface_secure::membership::MembershipSigner for LocalIdentity {
    fn device_id(&self) -> Result<String> {
        LocalIdentity::device_id(self)
    }

    fn sign_payload(&self, domain: &str, payload: &[u8]) -> Result<String> {
        LocalIdentity::sign_payload(self, domain, payload)
    }
}

pub fn digest(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
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
/// Checks the event and its author's signature; the author's `DeviceId` is its public key.
pub fn verify_signed_event(network_id: &str, event: &SignedEvent) -> Result<Event> {
    let body = check_event(network_id, event)?;
    verify_event_signature(event, &body.author)?;
    Ok(body)
}
/// Every check except the signature: size, hash, canonical encoding, network and shape.
pub fn check_event(network_id: &str, event: &SignedEvent) -> Result<Event> {
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
    validate_event_shape(&body)?;
    Ok(body)
}
pub fn verify_event_signature(event: &SignedEvent, public_key: &str) -> Result<()> {
    verify(
        public_key,
        &event.signature,
        &domain_message(EVENT_DOMAIN, &event.hash),
    )
}
pub fn verify_payload(
    public_key: &str,
    domain: &str,
    payload: &[u8],
    signature: &str,
) -> Result<()> {
    verify(public_key, signature, &payload_message(domain, payload)?)
}
fn payload_message(domain: &str, payload: &[u8]) -> Result<Vec<u8>> {
    ensure!(
        !domain.is_empty()
            && domain.len() <= 64
            && domain
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-'),
        "invalid signing domain"
    );
    Ok(format!("vibeface:message:v1:{domain}:{}", digest(payload)).into_bytes())
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
        event.seq > 0 && event.seq <= i64::MAX as u64,
        "invalid sequence"
    );
    ensure!(
        (event.seq == 1) == event.prev.is_none(),
        "invalid predecessor"
    );
    ensure!(event.created_at_ms >= 0, "invalid creation time");
    ensure!(
        event.deps.len() <= 1024 && event.blobs.len() <= 128,
        "too many dependencies"
    );
    ensure!(
        event.deps.windows(2).all(|w| w[0] < w[1]),
        "deps must be sorted and unique"
    );
    ensure!(
        event.blobs.windows(2).all(|w| w[0] < w[1]),
        "blobs must be sorted and unique"
    );
    for hash in event.deps.iter().chain(event.prev.iter()) {
        decode32(hash)?;
    }
    ensure!(
        event
            .prev
            .as_ref()
            .is_none_or(|prev| !event.deps.contains(prev)),
        "prev is not a dependency on another author"
    );
    for blob in &event.blobs {
        decode32(&blob.hash)?;
        ensure!(blob.size <= MAX_BLOB_BYTES as u64, "blob too large");
    }
    let id = |value: &str| vibeface_protocol::validate_id(value).map_err(anyhow::Error::msg);
    match &event.body {
        EventBody::MessagePosted {
            message_id,
            role,
            report_results,
            in_reply_to,
            status,
            ..
        } => {
            id(message_id)?;
            match role {
                Role::User => ensure!(
                    report_results.is_some() && in_reply_to.is_none() && status.is_none(),
                    "a user message carries only its reporting choice"
                ),
                Role::Assistant => {
                    ensure!(
                        report_results.is_none() && status.is_some(),
                        "an assistant reply carries its turn status"
                    );
                    in_reply_to.as_deref().map(id).transpose()?;
                }
            }
        }
        EventBody::ConversationOwner {
            scheduler_node,
            epoch,
        } => {
            decode32(scheduler_node)?;
            ensure!(*epoch > 0, "owner epoch starts at 1");
        }
        EventBody::WorkDispatched {
            work_id,
            trigger,
            dispatcher,
            call_ref,
            continues,
            parent,
            ..
        } => {
            id(work_id)?;
            let (kind, trigger_id) = trigger.split_once(':').unwrap_or_default();
            ensure!(matches!(kind, "message" | "work"), "invalid work trigger");
            id(trigger_id)?;
            id(call_ref)?;
            decode32(dispatcher)?;
            for work in continues.iter().chain(parent.iter()) {
                id(work)?;
            }
        }
        EventBody::WorkProgress { work_id, .. }
        | EventBody::WorkFinished { work_id, .. }
        | EventBody::ApprovalRequested { work_id, .. }
        | EventBody::ApprovalAnswered { work_id, .. } => id(work_id)?,
        EventBody::AutonomySet { version, .. } => {
            ensure!(*version > 0, "autonomy version starts at 1")
        }
        EventBody::ContextCheckpoint { covers, .. } => {
            for author in covers.keys() {
                decode32(author)?;
            }
        }
        _ => {}
    }
    Ok(())
}

#[cfg(test)]
mod tests {

    use super::*;
    #[test]
    fn signatures_pin_identity_and_original_bytes() -> Result<()> {
        let owner = LocalIdentity::generate();
        let other_node = LocalIdentity::generate();
        let state = owner.sign_network_fixture(1, vec![other_node.device_id()?])?;
        let view = state.project()?;
        assert_eq!(view.network_id, owner.fixture_network_id()?);
        assert!(view.node(&other_node.device_id()?).is_ok());
        assert!(view.node(&owner.device_id()?).is_err());
        let later = owner.sign_network_fixture(2, vec![])?;
        assert_eq!(state.genesis, later.genesis);
        let signed = owner.sign_payload("peer-request", b"challenge")?;
        verify_payload(&owner.device_id()?, "peer-request", b"challenge", &signed)?;
        assert!(
            verify_payload(&owner.device_id()?, "peer-response", b"challenge", &signed).is_err()
        );
        assert!(owner.sign_payload("bad:domain", b"challenge").is_err());
        let body = Event {
            version: 1,
            network_id: owner.fixture_network_id()?,
            scope_id: "c".into(),
            author: owner.device_id()?,
            seq: 1,
            prev: None,
            deps: vec![],
            created_at_ms: 0,
            body: EventBody::ConversationCreated {
                title: "test".into(),
            },
            blobs: vec![],
        };
        let event = owner.sign_event(body)?;
        verify_signed_event(&view.network_id, &event)?;
        let mut bad = event.clone();
        bad.canonical_body.push(' ');
        assert!(verify_signed_event(&view.network_id, &bad).is_err());
        assert!(verify_signed_event(&"0".repeat(64), &event).is_err());
        let mut forged = event.clone();
        forged.signature = LocalIdentity::generate().sign_payload("x", b"y")?;
        assert!(verify_signed_event(&view.network_id, &forged).is_err());
        Ok(())
    }

    #[test]
    fn message_roles_and_log_links_are_part_of_the_signed_shape() -> Result<()> {
        let identity = LocalIdentity::generate();
        let message = |role, report_results, status| EventBody::MessagePosted {
            message_id: "m-1".into(),
            role,
            text: "hi".into(),
            report_results,
            in_reply_to: None,
            status,
        };
        let mut event = Event {
            version: WIRE_VERSION,
            network_id: identity.fixture_network_id()?,
            author: identity.device_id()?,
            seq: 1,
            prev: None,
            deps: vec![digest(b"b"), digest(b"a"), digest(b"b")],
            scope_id: "conversation".into(),
            created_at_ms: 0,
            body: message(Role::User, Some(true), None),
            blobs: vec![],
        };
        let signed = identity.sign_event(event.clone())?;
        let verified = verify_signed_event(&event.network_id, &signed)?;
        assert_eq!(verified.deps.len(), 2);
        assert!(verified.deps.windows(2).all(|pair| pair[0] < pair[1]));
        event.body = message(Role::User, None, None);
        assert!(identity.sign_event(event.clone()).is_err());
        event.body = message(Role::Assistant, None, None);
        assert!(identity.sign_event(event.clone()).is_err());
        event.body = message(Role::Assistant, None, Some(ReplyStatus::Stopped));
        assert!(identity.sign_event(event.clone()).is_ok());
        event.seq = 2;
        assert!(identity.sign_event(event.clone()).is_err());
        event.prev = Some(digest(b"a"));
        assert!(
            identity.sign_event(event.clone()).is_err(),
            "prev must not repeat a dep"
        );
        event.prev = Some(digest(b"c"));
        assert!(identity.sign_event(event).is_ok());
        Ok(())
    }
}
