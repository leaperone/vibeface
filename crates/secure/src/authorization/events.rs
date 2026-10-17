use super::{ObjectHash, PublicKey, ResourceId, SpaceId};
use ed25519_dalek::{Signature, VerifyingKey};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

pub const EVENT_VERSION: u32 = 2;
pub const MAX_EVENT_BYTES: usize = 1_048_576;
pub const MAX_BLOB_BYTES: u64 = 64 * 1024 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(deny_unknown_fields)]
pub struct BlobRef {
    pub hash: String,
    pub size: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum EventBody {
    ConversationCreated {
        title: String,
    },
    MessagePosted {
        message_id: String,
        role: String,
        text: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        scheduler_node: Option<String>,
    },
    TaskCreated {
        task_id: String,
        title: String,
        goal: String,
        acceptance: String,
    },
    TaskChanged {
        task_id: String,
        status: String,
        detail: String,
    },
    DispatchRecorded {
        operation_id: String,
        task_id: Option<String>,
        target_node: String,
        session_id: String,
        action: Value,
    },
    ExecutionObserved {
        operation_id: String,
        task_id: Option<String>,
        target_node: String,
        session_id: String,
        status: String,
        detail: Value,
    },
    DecisionRecorded {
        decision_id: String,
        text: String,
        effective_constraints: Vec<String>,
        tool_configuration: Value,
    },
    NotificationRaised {
        notification_id: String,
        kind: String,
        text: String,
    },
    NotificationAcknowledged {
        notification_id: String,
        client_id: String,
    },
    ContextCheckpoint {
        summary: String,
        covered_heads: Vec<String>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum StoredTransport {
    DirectNoise {
        device: super::DeviceId,
    },
    GatewayNoise {
        device: super::DeviceId,
        gateway: super::DeviceId,
    },
    VerifiedPeer {
        origin: super::DeviceId,
    },
    LocalAgentAdapter {
        service: super::ServiceId,
        session: super::SessionRef,
    },
}

impl From<&super::TransportEvidence> for StoredTransport {
    fn from(value: &super::TransportEvidence) -> Self {
        use super::TransportEvidence;
        match value {
            TransportEvidence::DirectNoise { device } => Self::DirectNoise {
                device: device.clone(),
            },
            TransportEvidence::GatewayNoise { device, gateway } => Self::GatewayNoise {
                device: device.clone(),
                gateway: gateway.clone(),
            },
            TransportEvidence::VerifiedPeer { origin } => Self::VerifiedPeer {
                origin: origin.clone(),
            },
            TransportEvidence::LocalAgentAdapter { service, session } => Self::LocalAgentAdapter {
                service: service.clone(),
                session: session.clone(),
            },
        }
    }
}

impl StoredTransport {
    pub fn into_evidence(self) -> super::TransportEvidence {
        use super::TransportEvidence;
        match self {
            Self::DirectNoise { device } => TransportEvidence::DirectNoise { device },
            Self::GatewayNoise { device, gateway } => {
                TransportEvidence::GatewayNoise { device, gateway }
            }
            Self::VerifiedPeer { origin } => TransportEvidence::VerifiedPeer { origin },
            Self::LocalAgentAdapter { service, session } => {
                TransportEvidence::LocalAgentAdapter { service, session }
            }
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct AdmissionProof {
    pub invocation: super::Signed<super::Invocation>,
    pub source_body: Value,
    pub challenge: super::FreshnessChallenge,
    pub receipts: Vec<super::Signed<super::FreshnessReceipt>>,
    pub admitted_at_ms: u64,
    pub transport: StoredTransport,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum EventCause {
    Invocation {
        admission: AdmissionProof,
    },
    Derived {
        admission: AdmissionProof,
        source: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Event {
    pub version: u32,
    #[serde(rename = "space_id")]
    pub network_id: String,
    pub resource_id: super::ResourceId,
    pub cause: EventCause,
    pub scope_id: String,
    pub author: String,
    pub sequence: u64,
    pub previous: Option<String>,
    pub parents: Vec<String>,
    pub created_at_ms: i64,
    pub body: EventBody,
    pub blobs: Vec<BlobRef>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SignedEvent {
    pub hash: String,
    pub canonical_body: String,
    pub signature: String,
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen)]
pub struct VerifiedEvent {
    envelope: SignedEvent,
    event: Event,
    envelope_context_hash: ObjectHash,
}

impl VerifiedEvent {
    pub fn event(&self) -> &Event {
        &self.event
    }
    pub fn envelope(&self) -> &SignedEvent {
        &self.envelope
    }
    pub fn context_hash(&self) -> &ObjectHash {
        &self.envelope_context_hash
    }
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen)]
impl VerifiedEvent {
    #[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen(getter))]
    pub fn hash(&self) -> String {
        self.envelope.hash.clone()
    }

    #[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen(js_name = bodyJson))]
    pub fn body_json(&self) -> String {
        self.envelope.canonical_body.clone()
    }

    #[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen(js_name = envelopeContextHash))]
    pub fn envelope_context_hash(&self) -> String {
        self.envelope_context_hash.as_str().into()
    }
}

fn raw_hash(value: &str) -> crate::Result<[u8; 32]> {
    if value.len() != 64
        || !value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        return Err("expected a canonical event hash".into());
    }
    let mut bytes = [0; 32];
    hex::decode_to_slice(value, &mut bytes).map_err(|_| "invalid event hash")?;
    Ok(bytes)
}

fn validate_numbers(value: &Value) -> crate::Result<()> {
    const MAX_SAFE: u64 = 9_007_199_254_740_991;
    match value {
        Value::Number(number) => {
            if number.as_u64().is_some_and(|n| n > MAX_SAFE)
                || number.as_i64().is_some_and(|n| n.unsigned_abs() > MAX_SAFE)
            {
                return Err("event integer exceeds JavaScript precision".into());
            }
        }
        Value::Array(values) => {
            for value in values {
                validate_numbers(value)?;
            }
        }
        Value::Object(values) => {
            for value in values.values() {
                validate_numbers(value)?;
            }
        }
        _ => {}
    }
    Ok(())
}

pub fn validate_event_shape(event: &Event) -> crate::Result<()> {
    if event.version != EVENT_VERSION {
        return Err("unsupported event version".into());
    }
    SpaceId::parse(&event.network_id).map_err(|e| e.to_string())?;
    raw_hash(&event.author)?;
    if event.scope_id.is_empty() || event.scope_id.len() > 512 {
        return Err("invalid event scope".into());
    }
    if event.sequence == 0 || (event.sequence == 1) != event.previous.is_none() {
        return Err("invalid event sequence or predecessor".into());
    }
    if event.created_at_ms < 0 {
        return Err("negative event creation time".into());
    }
    if event.parents.len() > 1024 || event.blobs.len() > 128 {
        return Err("too many event dependencies".into());
    }
    if !event.parents.windows(2).all(|w| w[0] < w[1])
        || !event.blobs.windows(2).all(|w| w[0] < w[1])
    {
        return Err("event dependencies are not sorted and unique".into());
    }
    for hash in event.parents.iter().chain(&event.previous) {
        raw_hash(hash)?;
    }
    for blob in &event.blobs {
        raw_hash(&blob.hash)?;
        if blob.size > MAX_BLOB_BYTES {
            return Err("event blob is too large".into());
        }
    }
    if let EventBody::ContextCheckpoint { covered_heads, .. } = &event.body
        && !covered_heads
            .iter()
            .all(|head| event.parents.contains(head))
    {
        return Err("checkpoint head must be an event parent".into());
    }
    validate_numbers(&serde_json::to_value(event).map_err(|_| "event serialization failed")?)?;
    Ok(())
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen(js_name = verifySignedEvent))]
pub fn verify_signed_event(
    envelope_json: &str,
    expected_space: &str,
    expected_resource: &str,
    author_public_key: &str,
) -> crate::Result<VerifiedEvent> {
    if envelope_json.len() > MAX_EVENT_BYTES * 6 + 1024 {
        return Err("event envelope is too large".into());
    }
    let envelope: SignedEvent =
        serde_json::from_str(envelope_json).map_err(|_| "invalid signed event JSON")?;
    if envelope.canonical_body.len() > MAX_EVENT_BYTES {
        return Err("event is too large".into());
    }
    raw_hash(&envelope.hash)?;
    if hex::encode(Sha256::digest(envelope.canonical_body.as_bytes())) != envelope.hash {
        return Err("event hash mismatch".into());
    }
    let event: Event =
        serde_json::from_str(&envelope.canonical_body).map_err(|_| "invalid event JSON")?;
    validate_event_shape(&event)?;
    if serde_json::to_string(&event).map_err(|_| "event serialization failed")?
        != envelope.canonical_body
    {
        return Err("noncanonical event encoding".into());
    }
    let space = SpaceId::parse(expected_space).map_err(|e| e.to_string())?;
    let resource = ResourceId::parse(expected_resource).map_err(|e| e.to_string())?;
    if event.network_id != space.as_str() || event.resource_id != resource {
        return Err("event scope differs from trusted scope".into());
    }
    let public_key = PublicKey::parse(author_public_key).map_err(|e| e.to_string())?;
    let key_bytes = raw_hash(public_key.as_str())?;
    if event.author != hex::encode(Sha256::digest(key_bytes)) {
        return Err("event author differs from trusted device".into());
    }
    if envelope.signature.len() != 128
        || !envelope
            .signature
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        return Err("invalid event signature encoding".into());
    }
    let signature_bytes =
        hex::decode(&envelope.signature).map_err(|_| "invalid event signature encoding")?;
    let signature =
        Signature::from_slice(&signature_bytes).map_err(|_| "invalid event signature size")?;
    let key = VerifyingKey::from_bytes(&key_bytes).map_err(|_| "invalid event public key")?;
    let mut signed_bytes = b"vibeface:event:v2:".to_vec();
    signed_bytes.extend_from_slice(envelope.hash.as_bytes());
    key.verify_strict(&signed_bytes, &signature)
        .map_err(|_| "invalid event signature")?;
    let envelope_context_hash = ObjectHash::body(&envelope).map_err(|e| e.to_string())?;
    Ok(VerifiedEvent {
        envelope,
        event,
        envelope_context_hash,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Identity, authorization::*};
    use ed25519_dalek::Signer;
    use serde_json::json;

    fn fixture() -> (Event, Identity) {
        let identity = Identity::from_secret_hex(&hex::encode([7; 32])).unwrap();
        let key = PublicKey::of(&identity);
        let account = AccountId::from_root(&key);
        let space = SpaceId::personal(&account);
        let resource = ResourceId::from_bytes([8; 32]);
        let audience = Audience {
            service: ServiceId::from_key(&key),
            device: DeviceId::from_key(&key),
        };
        let source_body = json!({"method":"create_task","space_id":space,"resource_id":resource,"authorization_id":"task_1",
            "conversation_id":"chat","task_id":"task","title":"signed task","goal":"goal","acceptance":"acceptance"});
        let policies = vec![PolicyStamp {
            domain: PolicyDomain::Space(space.clone()),
            authority_epoch: 0,
            sequence: 1,
            head: ObjectHash::body(&"policy").unwrap(),
        }];
        let invocation = Signed::sign(
            Invocation {
                version: AUTH_VERSION,
                actor: Actor::Account(account.clone()),
                device: audience.device.clone(),
                initiator: Actor::Account(account),
                initiating_grant: None,
                resource: ResourceRef {
                    space: space.clone(),
                    resource: resource.clone(),
                },
                action: Action::TaskCreate,
                audience: audience.clone(),
                presenter: audience.device.clone(),
                operation_id: "task_1".into(),
                body_hash: ObjectHash::body(&source_body).unwrap(),
                grant_leaf: ObjectHash::body(&"grant").unwrap(),
                expected_policies: policies.clone(),
                context: InvocationContext {
                    conversation: Some("chat".into()),
                    workspace_resource: Some(resource.clone()),
                    ..Default::default()
                },
                nonce: Nonce::from_bytes([6; 32]),
                not_before_ms: 0,
                expires_at_ms: 1000,
            },
            &identity,
        )
        .unwrap();
        let challenge = FreshnessChallenge {
            version: AUTH_VERSION,
            stage: CheckStage::Admit,
            invocation_hash: invocation.hash().unwrap(),
            operation_id: "task_1".into(),
            body_hash: invocation.payload.body_hash.clone(),
            audience: audience.clone(),
            presenter: audience.device.clone(),
            policies,
            nonce: Nonce::from_bytes([9; 32]),
            issued_at_ms: 1,
            expires_at_ms: 1000,
        };
        (
            Event {
                version: EVENT_VERSION,
                network_id: space.as_str().into(),
                resource_id: resource,
                cause: EventCause::Invocation {
                    admission: AdmissionProof {
                        invocation,
                        source_body,
                        challenge,
                        receipts: vec![],
                        admitted_at_ms: 1,
                        transport: StoredTransport::DirectNoise {
                            device: audience.device,
                        },
                    },
                },
                scope_id: "chat".into(),
                author: identity.node_id(),
                sequence: 1,
                previous: None,
                parents: vec![],
                created_at_ms: 1,
                body: EventBody::TaskCreated {
                    task_id: "task".into(),
                    title: "signed task".into(),
                    goal: "goal".into(),
                    acceptance: "acceptance".into(),
                },
                blobs: vec![],
            },
            identity,
        )
    }

    fn sign_bytes(body: String, identity: &Identity) -> SignedEvent {
        let hash = hex::encode(Sha256::digest(body.as_bytes()));
        let mut bytes = b"vibeface:event:v2:".to_vec();
        bytes.extend_from_slice(hash.as_bytes());
        SignedEvent {
            hash,
            canonical_body: body,
            signature: hex::encode(identity.key.sign(&bytes).to_bytes()),
        }
    }

    fn verify(
        event: &Event,
        envelope: &SignedEvent,
        identity: &Identity,
    ) -> crate::Result<VerifiedEvent> {
        verify_signed_event(
            &serde_json::to_string(envelope).unwrap(),
            &event.network_id,
            event.resource_id.as_str(),
            &identity.public_key_hex(),
        )
    }

    #[test]
    fn verifies_raw_event_signature_and_exact_envelope_context() {
        let (event, identity) = fixture();
        let envelope = sign_bytes(serde_json::to_string(&event).unwrap(), &identity);
        let verified = verify(&event, &envelope, &identity).unwrap();
        assert_eq!(verified.event(), &event);
        assert_eq!(verified.envelope(), &envelope);
        assert_eq!(verified.hash(), envelope.hash);
        assert_eq!(verified.body_json(), envelope.canonical_body);
        assert_eq!(
            verified.context_hash(),
            &ObjectHash::body(&envelope).unwrap()
        );
        let encoded = serde_json::to_string(&envelope).unwrap();
        assert!(
            verify_signed_event(
                &encoded,
                &SpaceId::team(&PublicKey::of(&identity)).to_string(),
                event.resource_id.as_str(),
                &identity.public_key_hex()
            )
            .is_err()
        );
        assert!(
            verify_signed_event(
                &encoded,
                &event.network_id,
                ResourceId::from_bytes([99; 32]).as_str(),
                &identity.public_key_hex()
            )
            .is_err()
        );
        let wrong = Identity::from_secret_hex(&hex::encode([8; 32])).unwrap();
        assert!(verify(&event, &envelope, &wrong).is_err());
    }

    #[test]
    fn rejects_hash_signature_noncanonical_unknown_duplicate_and_unsafe_values() {
        let (event, identity) = fixture();
        let envelope = sign_bytes(serde_json::to_string(&event).unwrap(), &identity);
        let mut changed = envelope.clone();
        changed.canonical_body = changed
            .canonical_body
            .replace("signed task", "altered task");
        assert!(verify(&event, &changed, &identity).is_err());
        changed = envelope.clone();
        changed.signature = "00".repeat(64);
        assert!(verify(&event, &changed, &identity).is_err());
        changed = sign_bytes(
            serde_json::to_string(
                &serde_json::from_str::<Value>(&envelope.canonical_body).unwrap(),
            )
            .unwrap(),
            &identity,
        );
        assert!(verify(&event, &changed, &identity).is_err());
        changed = sign_bytes(
            envelope.canonical_body.replacen(
                "\"scope_id\":\"chat\"",
                "\"scope_id\":\"other\",\"scope_id\":\"chat\"",
                1,
            ),
            &identity,
        );
        assert!(verify(&event, &changed, &identity).is_err());
        changed = sign_bytes(
            envelope
                .canonical_body
                .replacen("\"body\":{", "\"body\":{\"unknown\":true,", 1),
            &identity,
        );
        assert!(verify(&event, &changed, &identity).is_err());
        let mut unsafe_event = event.clone();
        unsafe_event.sequence = 9_007_199_254_740_992;
        unsafe_event.previous = Some("aa".repeat(32));
        changed = sign_bytes(serde_json::to_string(&unsafe_event).unwrap(), &identity);
        assert!(verify(&event, &changed, &identity).is_err());
        let mut invalid_version = event.clone();
        invalid_version.version = 1;
        changed = sign_bytes(serde_json::to_string(&invalid_version).unwrap(), &identity);
        assert!(verify(&event, &changed, &identity).is_err());
        let unknown_envelope =
            serde_json::to_string(&envelope)
                .unwrap()
                .replacen('{', "{\"unknown\":true,", 1);
        assert!(
            verify_signed_event(
                &unknown_envelope,
                &event.network_id,
                event.resource_id.as_str(),
                &identity.public_key_hex()
            )
            .is_err()
        );
    }
}
