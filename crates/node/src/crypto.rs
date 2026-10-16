use crate::model::*;
use anyhow::{Context as _, Result, ensure};
use ed25519_dalek::{Signature, Signer, SigningKey, VerifyingKey};
use rand::rngs::OsRng;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;

const EVENT_DOMAIN: &[u8] = b"vibeface:event:v1:";
const TRUST_DOMAIN: &[u8] = b"vibeface:trust:v1:";

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
    pub fn network_id(&self) -> Result<NetworkId> {
        self.node_id()
    }
    pub fn certificate(&self, roles: Vec<NodeRole>) -> Result<NodeCertificate> {
        Ok(NodeCertificate {
            node_id: self.node_id()?,
            public_key: self.public_key_hex()?,
            roles,
            revoked: false,
        })
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
    pub fn sign_trust_bundle(
        &self,
        revision: u64,
        mut nodes: Vec<NodeCertificate>,
    ) -> Result<TrustBundle> {
        nodes.sort_by(|a, b| a.node_id.cmp(&b.node_id));
        let mut bundle = TrustBundle {
            version: WIRE_VERSION,
            network_id: self.network_id()?,
            owner_public_key: self.public_key_hex()?,
            revision,
            nodes,
            signature: String::new(),
        };
        validate_bundle_shape(&bundle)?;
        bundle.signature = hex::encode(
            self.key()?
                .sign(&domain_message(
                    TRUST_DOMAIN,
                    &digest(&unsigned_bundle(&bundle)?),
                ))
                .to_bytes(),
        );
        Ok(bundle)
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
fn unsigned_bundle(bundle: &TrustBundle) -> Result<Vec<u8>> {
    let mut b = bundle.clone();
    b.signature.clear();
    Ok(serde_json::to_vec(&b)?)
}
fn validate_bundle_shape(bundle: &TrustBundle) -> Result<()> {
    ensure!(
        bundle.version == WIRE_VERSION,
        "unsupported trust bundle version"
    );
    ensure!(bundle.revision > 0, "trust revision must be positive");
    ensure!(
        bundle.network_id == fingerprint(&bundle.owner_public_key)?,
        "network owner fingerprint mismatch"
    );
    ensure!(bundle.nodes.len() <= 4096, "too many node certificates");
    let mut previous = None;
    for node in &bundle.nodes {
        ensure!(
            node.node_id == fingerprint(&node.public_key)?,
            "node fingerprint mismatch"
        );
        ensure!(
            previous.as_ref().is_none_or(|p: &String| p < &node.node_id),
            "duplicate or unsorted certificates"
        );
        ensure!(!node.roles.is_empty(), "node must have a role");
        let roles: BTreeSet<String> = node.roles.iter().map(|r| format!("{r:?}")).collect();
        ensure!(roles.len() == node.roles.len(), "duplicate role");
        previous = Some(node.node_id.clone());
    }
    Ok(())
}
pub fn verify_trust_bundle(bundle: &TrustBundle) -> Result<()> {
    validate_bundle_shape(bundle)?;
    verify(
        &bundle.owner_public_key,
        &bundle.signature,
        &domain_message(TRUST_DOMAIN, &digest(&unsigned_bundle(bundle)?)),
    )
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
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn signatures_pin_identity_and_original_bytes() -> Result<()> {
        let owner = LocalIdentity::generate();
        let bundle =
            owner.sign_trust_bundle(1, vec![owner.certificate(vec![NodeRole::Replica])?])?;
        verify_trust_bundle(&bundle)?;
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
            network_id: owner.network_id()?,
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
        let event = owner.sign_event(body)?;
        verify_signed_event(&bundle.network_id, &event, &owner.public_key_hex()?)?;
        let mut bad = event.clone();
        bad.canonical_body.push(' ');
        assert!(verify_signed_event(&bundle.network_id, &bad, &owner.public_key_hex()?).is_err());
        assert!(verify_signed_event(&"0".repeat(64), &event, &owner.public_key_hex()?).is_err());
        let other = LocalIdentity::generate();
        assert!(verify_signed_event(&bundle.network_id, &event, &other.public_key_hex()?).is_err());
        Ok(())
    }
}
