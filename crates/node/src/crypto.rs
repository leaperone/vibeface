use crate::model::*;
use anyhow::{Context as _, Result, ensure};
use ed25519_dalek::{Signature, Signer, SigningKey, VerifyingKey};
use rand::rngs::OsRng;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

const EVENT_DOMAIN: &[u8] = b"vibeface:event:v2:";

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
    pub fn node_id(&self) -> Result<NodeId> {
        fingerprint(&self.public_key_hex()?)
    }
    pub fn sign_payload(&self, domain: &str, payload: &[u8]) -> Result<String> {
        let message = payload_message(domain, payload)?;
        Ok(hex::encode(self.key()?.sign(&message).to_bytes()))
    }
    pub fn sign_event(&self, mut event: Event) -> Result<SignedEvent> {
        ensure!(
            event.author == self.node_id()?,
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
        "event belongs to another space"
    );
    ensure!(
        body.author == fingerprint(public_key)?,
        "author fingerprint mismatch"
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
    vibeface_secure::authorization::events::validate_event_shape(event).map_err(anyhow::Error::msg)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn admission(
        owner: &LocalIdentity,
        space: &str,
        resource: &vibeface_secure::authorization::ResourceId,
    ) -> Result<AdmissionProof> {
        use vibeface_secure::authorization::*;
        let identity = vibeface_secure::Identity::from_secret_hex(&owner.secret_key_hex)
            .map_err(anyhow::Error::msg)?;
        let key = PublicKey::of(&identity);
        let actor = Actor::Account(AccountId::from_root(&key));
        let device = DeviceId::from_key(&key);
        let audience = Audience {
            service: ServiceId::from_key(&key),
            device: device.clone(),
        };
        let source_body = serde_json::json!({"method":"create_conversation","space_id":space,"authorization_id":"test","conversation_id":"c","title":"test"});
        let invocation = Signed::sign(
            Invocation {
                version: AUTH_VERSION,
                actor: actor.clone(),
                device: device.clone(),
                initiator: actor,
                initiating_grant: None,
                resource: ResourceRef {
                    space: SpaceId::parse(space)?,
                    resource: resource.clone(),
                },
                action: Action::ConversationCreate,
                audience: audience.clone(),
                presenter: device.clone(),
                operation_id: "test".into(),
                body_hash: ObjectHash::body(&source_body)?,
                grant_leaf: ObjectHash::parse(&format!("h2_{}", "0".repeat(64)))?,
                expected_policies: vec![],
                context: InvocationContext::default(),
                nonce: Nonce::from_bytes([1; 32]),
                not_before_ms: 0,
                expires_at_ms: 100,
            },
            &identity,
        )?;
        let challenge = FreshnessChallenge {
            version: AUTH_VERSION,
            stage: CheckStage::Admit,
            invocation_hash: invocation.hash()?,
            operation_id: "test".into(),
            body_hash: invocation.payload.body_hash.clone(),
            audience,
            presenter: device.clone(),
            policies: vec![],
            nonce: Nonce::from_bytes([2; 32]),
            issued_at_ms: 0,
            expires_at_ms: 100,
        };
        Ok(AdmissionProof {
            invocation,
            source_body,
            challenge,
            receipts: vec![],
            admitted_at_ms: 0,
            transport: StoredTransport::DirectNoise { device },
        })
    }
    #[test]
    fn signatures_pin_identity_and_original_bytes() -> Result<()> {
        let owner = LocalIdentity::generate();
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
        let space_id = format!("spc2_{}", owner.node_id()?);
        let resource_id = vibeface_secure::authorization::ResourceId::from_bytes([7; 32]);
        let body = Event {
            version: WIRE_VERSION,
            network_id: space_id.clone(),
            cause: EventCause::Invocation {
                admission: admission(&owner, &space_id, &resource_id)?,
            },
            resource_id,
            scope_id: "c".into(),
            author: owner.node_id()?,
            sequence: 1,
            previous: None,
            parents: vec![],
            created_at_ms: 0,
            body: EventBody::ConversationCreated {
                title: "test".into(),
            },
            blobs: vec![],
        };
        let mut old_body = body.clone();
        old_body.version = 1;
        assert!(owner.sign_event(old_body).is_err());
        let mut old_space = body.clone();
        old_space.network_id = owner.node_id()?;
        assert!(owner.sign_event(old_space).is_err());
        let mut encoded = serde_json::to_value(&body)?;
        assert_eq!(encoded["space_id"], space_id);
        assert!(encoded.get("network_id").is_none());
        let space = encoded.as_object_mut().unwrap().remove("space_id").unwrap();
        encoded["network_id"] = space;
        assert!(serde_json::from_value::<Event>(encoded).is_err());
        let event = owner.sign_event(body)?;
        verify_signed_event(&space_id, &event, &owner.public_key_hex()?)?;
        let mut legacy = event.clone();
        legacy.signature = hex::encode(
            owner
                .key()?
                .sign(&domain_message(b"vibeface:event:v1:", &event.hash))
                .to_bytes(),
        );
        assert!(verify_signed_event(&space_id, &legacy, &owner.public_key_hex()?).is_err());
        let mut bad = event.clone();
        bad.canonical_body.push(' ');
        assert!(verify_signed_event(&space_id, &bad, &owner.public_key_hex()?).is_err());
        assert!(
            verify_signed_event(
                &format!("spc2_{}", "0".repeat(64)),
                &event,
                &owner.public_key_hex()?
            )
            .is_err()
        );
        let other = LocalIdentity::generate();
        assert!(verify_signed_event(&space_id, &event, &other.public_key_hex()?).is_err());
        Ok(())
    }
}
