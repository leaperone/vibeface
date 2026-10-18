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

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct BotProject {
    pub target_node: String,
    pub cwd: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum EventBody {
    ConversationCreated {
        title: String,
    },
    BotCreated {
        bot_id: String,
        name: String,
        instructions: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        project: Option<BotProject>,
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
        session_id: Option<String>,
        action: Value,
    },
    ExecutionObserved {
        operation_id: String,
        task_id: Option<String>,
        target_node: String,
        session_id: Option<String>,
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
    DirectedGateway {
        header: super::Signed<super::gateway_forward::GatewayForwardHeader>,
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
    pub fn existing_evidence(&self) -> super::Result<super::TransportEvidence> {
        use super::TransportEvidence;
        Ok(match self {
            Self::DirectNoise { device } => TransportEvidence::DirectNoise {
                device: device.clone(),
            },
            Self::GatewayNoise { device, gateway } => TransportEvidence::GatewayNoise {
                device: device.clone(),
                gateway: gateway.clone(),
            },
            Self::DirectedGateway { .. } => {
                return Err(super::AuthorizationError::InvalidProof(
                    "stored Gateway provenance cannot become live transport evidence",
                ));
            }
            Self::VerifiedPeer { origin } => TransportEvidence::VerifiedPeer {
                origin: origin.clone(),
            },
            Self::LocalAgentAdapter { service, session } => TransportEvidence::LocalAgentAdapter {
                service: service.clone(),
                session: session.clone(),
            },
        })
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

pub fn validate_bot_definition(
    bot_id: &str,
    conversation_id: &str,
    name: &str,
    instructions: &str,
    project: Option<&BotProject>,
) -> crate::Result<()> {
    let valid_id = |value: &str| {
        !value.is_empty()
            && value.len() <= 128
            && value
                .chars()
                .all(|character| character.is_ascii_alphanumeric() || "-_.:".contains(character))
    };
    if !valid_id(bot_id) || !valid_id(conversation_id) {
        return Err("invalid Bot or conversation id".into());
    }
    if name.trim().is_empty() || name.len() > 512 || name.chars().any(char::is_control) {
        return Err("invalid Bot name".into());
    }
    if instructions.len() > 8192
        || serde_json::to_vec(&(name, instructions, project))
            .map_err(|_| "invalid Bot definition")?
            .len()
            > 12_000
    {
        return Err("Bot definition exceeds its size limit".into());
    }
    if let Some(project) = project {
        let path = std::path::Path::new(&project.cwd);
        if !valid_id(&project.target_node)
            || project.cwd.len() > 4096
            || !path.is_absolute()
            || project.cwd.chars().any(char::is_control)
            || path.components().any(|component| {
                matches!(
                    component,
                    std::path::Component::ParentDir | std::path::Component::CurDir
                )
            })
        {
            return Err("Bot project requires a target and an absolute directory".into());
        }
    }
    Ok(())
}

pub fn bot_creation_facts(event: &Event) -> super::Result<super::RequestFacts> {
    use super::{Action, Actor, AuthorizationError, InvocationContext, RequestFacts, ResourceRef};
    let EventBody::BotCreated {
        bot_id,
        name,
        instructions,
        project,
    } = &event.body
    else {
        return Err(AuthorizationError::InvalidProof(
            "event is not a Bot creation",
        ));
    };
    let EventCause::Invocation { admission } = &event.cause else {
        return Err(AuthorizationError::InvalidProof(
            "Bot creation requires an original invocation",
        ));
    };
    validate_bot_definition(
        bot_id,
        &event.scope_id,
        name,
        instructions,
        project.as_ref(),
    )
    .map_err(|_| AuthorizationError::InvalidProof("invalid Bot definition"))?;
    let body = &admission.source_body;
    let fields = body.as_object().ok_or(AuthorizationError::InvalidProof(
        "invalid Bot creation body",
    ))?;
    let invocation = &admission.invocation.payload;
    let resource = ResourceRef {
        space: SpaceId::parse(&event.network_id)?,
        resource: event.resource_id.clone(),
    };
    let context = InvocationContext {
        conversation: Some(event.scope_id.clone()),
        workspace_resource: Some(event.resource_id.clone()),
        ..Default::default()
    };
    if !fields.keys().all(|key| {
        matches!(
            key.as_str(),
            "method"
                | "space_id"
                | "resource_id"
                | "authorization_id"
                | "bot_id"
                | "conversation_id"
                | "name"
                | "instructions"
                | "project"
        )
    }) || body["method"] != "create_bot"
        || body["space_id"].as_str() != Some(event.network_id.as_str())
        || body["resource_id"].as_str() != Some(event.resource_id.as_str())
        || body["authorization_id"].as_str() != Some(invocation.operation_id.as_str())
        || body["bot_id"].as_str() != Some(bot_id)
        || body["conversation_id"].as_str() != Some(event.scope_id.as_str())
        || body["name"].as_str() != Some(name)
        || body["instructions"].as_str() != Some(instructions)
        || body["project"]
            != serde_json::to_value(project)
                .map_err(|_| AuthorizationError::InvalidProof("invalid Bot project"))?
        || invocation.resource != resource
        || invocation.action != Action::ConversationCreate
        || !matches!(invocation.actor, Actor::Account(_))
        || invocation.body_hash != ObjectHash::body(body)?
        || invocation.context != context
        || !event.blobs.is_empty()
    {
        return Err(AuthorizationError::InvalidProof(
            "Bot creation differs from its Account admission",
        ));
    }
    Ok(RequestFacts {
        resource,
        action: Action::ConversationCreate,
        audience: invocation.audience.clone(),
        operation_id: invocation.operation_id.clone(),
        body_hash: ObjectHash::body(body)?,
        context,
    })
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
    if matches!(event.body, EventBody::BotCreated { .. }) {
        bot_creation_facts(event).map_err(|error| error.to_string())?;
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
    if matches!(event.body, EventBody::BotCreated { .. }) {
        let facts = bot_creation_facts(&event).map_err(|error| error.to_string())?;
        if facts.audience.device != super::DeviceId::from_key(&public_key)
            || facts.audience.service != super::ServiceId::from_key(&public_key)
        {
            return Err("Bot author differs from its admitted Project host".into());
        }
    }
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
    crate::signature_verification::verify_cached(
        key.as_bytes(),
        "vibeface:event:v2",
        &signed_bytes,
        &signature_bytes,
        || {
            key.verify_strict(&signed_bytes, &signature)
                .map_err(|_| "invalid event signature".to_owned())
        },
    )?;
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

    fn admitted_bot() -> (Event, Identity, VerifiedPolicySet) {
        let fixture = execution_observation::tests::retained_history_fixture();
        let at = fixture.invocation.payload.not_before_ms;
        let state = fixture.source.policy_snapshot.clone();
        let project = fixture.invocation.payload.resource.clone();
        let audience = fixture.invocation.payload.audience.clone();
        let project_config = BotProject {
            target_node: fixture.project_node.node_id(),
            cwd: "/workspace/project".into(),
        };
        let body = json!({
            "method":"create_bot", "space_id":project.space, "resource_id":project.resource,
            "authorization_id":"create-review-bot", "bot_id":"review-bot",
            "conversation_id":"bot-chat", "name":"Review", "instructions":"Check the requested changes.",
            "project":project_config,
        });
        let context = InvocationContext {
            conversation: Some("bot-chat".into()),
            workspace_resource: Some(project.resource.clone()),
            ..Default::default()
        };
        let facts = RequestFacts {
            resource: project.clone(),
            action: Action::ConversationCreate,
            audience: audience.clone(),
            operation_id: "create-review-bot".into(),
            body_hash: ObjectHash::body(&body).unwrap(),
            context: context.clone(),
        };
        let mut payload = fixture.invocation.payload.clone();
        payload.operation_id = facts.operation_id.clone();
        payload.action = facts.action;
        payload.body_hash = facts.body_hash.clone();
        payload.context = context;
        payload.expected_policies.clear();
        payload.nonce = Nonce::random().unwrap();
        let transport = TransportEvidence::DirectNoise {
            device: payload.device.clone(),
        };
        let initial = Signed::sign(payload.clone(), &fixture.client).unwrap();
        let AuthorizationError::NeedPolicySync(heads) = state
            .verify_invocation(&initial, &facts, &transport, at)
            .unwrap_err()
        else {
            panic!("Bot fixture must discover current policy heads")
        };
        payload.expected_policies = heads;
        let invocation = Signed::sign(payload, &fixture.client).unwrap();
        let authorization = state
            .verify_invocation(&invocation, &facts, &transport, at)
            .unwrap();
        let challenge = FreshnessChallenge::new(
            &authorization,
            CheckStage::Admit,
            Nonce::random().unwrap(),
            at,
            at + 1000,
        )
        .unwrap();
        let receipts = challenge
            .policies
            .iter()
            .map(|current| {
                let authority = match &current.domain {
                    PolicyDomain::Account(id) => &state.account(id).unwrap().authority,
                    PolicyDomain::Space(id) => &state.space(id).unwrap().authority,
                };
                assert_eq!(authority.public_key, PublicKey::of(&fixture.project_node));
                Signed::sign(
                    FreshnessReceipt {
                        version: AUTH_VERSION,
                        current: current.clone(),
                        challenge_hash: challenge.hash().unwrap(),
                        issued_at_ms: at,
                        expires_at_ms: at + 1000,
                    },
                    &fixture.project_node,
                )
                .unwrap()
            })
            .collect::<Vec<_>>();
        let fresh = state
            .verify_freshness(&authorization, &challenge, &receipts, at)
            .unwrap();
        state.admit_operation(&authorization, &fresh, at).unwrap();
        let event = Event {
            version: EVENT_VERSION,
            network_id: project.space.as_str().into(),
            resource_id: project.resource,
            cause: EventCause::Invocation {
                admission: AdmissionProof {
                    invocation,
                    source_body: body,
                    challenge,
                    receipts,
                    admitted_at_ms: at,
                    transport: StoredTransport::from(&transport),
                },
            },
            scope_id: "bot-chat".into(),
            author: fixture.project_node.node_id(),
            sequence: 1,
            previous: None,
            parents: vec![],
            created_at_ms: at as i64,
            blobs: vec![],
            body: EventBody::BotCreated {
                bot_id: "review-bot".into(),
                name: "Review".into(),
                instructions: "Check the requested changes.".into(),
                project: Some(project_config),
            },
        };
        (event, fixture.project_node, state)
    }

    #[test]
    fn bot_creation_preserves_its_real_account_admission() {
        let (event, node, state) = admitted_bot();
        let envelope = sign_bytes(serde_json::to_string(&event).unwrap(), &node);
        let verified = verify(&event, &envelope, &node).unwrap();
        let facts = bot_creation_facts(verified.event()).unwrap();
        let EventCause::Invocation { admission } = &verified.event().cause else {
            unreachable!()
        };
        state.verify_original_admission(admission, &facts).unwrap();
        assert_eq!(facts.action, Action::ConversationCreate);
        assert_eq!(facts.context.conversation.as_deref(), Some("bot-chat"));
        assert!(facts.context.task.is_none());
        assert!(facts.context.sdk_session.is_none());
        assert!(facts.context.execution_session.is_none());
    }

    #[test]
    fn bot_creation_rejects_changed_fields_wrong_scope_and_service_origins() {
        let (event, node, state) = admitted_bot();
        for field in [
            "bot_id",
            "name",
            "instructions",
            "project",
            "scope",
            "actor",
            "method",
            "derived",
        ] {
            let mut changed = event.clone();
            let EventBody::BotCreated {
                bot_id,
                name,
                instructions,
                project,
            } = &mut changed.body
            else {
                unreachable!()
            };
            match field {
                "bot_id" => *bot_id = "other-bot".into(),
                "name" => *name = "Other name".into(),
                "instructions" => *instructions = "Different instructions".into(),
                "project" => project.as_mut().unwrap().cwd = "/other".into(),
                "scope" => changed.scope_id = "other-chat".into(),
                "actor" => {
                    let EventCause::Invocation { admission } = &mut changed.cause else {
                        unreachable!()
                    };
                    admission.invocation.payload.actor =
                        Actor::Service(ServiceId::from_key(&PublicKey::of(&node)));
                }
                "method" => {
                    let EventCause::Invocation { admission } = &mut changed.cause else {
                        unreachable!()
                    };
                    admission.source_body["method"] = json!("create_conversation");
                }
                "derived" => {
                    let EventCause::Invocation { admission } = changed.cause else {
                        unreachable!()
                    };
                    changed.cause = EventCause::Derived {
                        admission,
                        source: "scheduler".into(),
                    };
                }
                _ => unreachable!(),
            }
            let signed = sign_bytes(serde_json::to_string(&changed).unwrap(), &node);
            assert!(
                verify(&changed, &signed, &node).is_err(),
                "accepted changed {field}"
            );
        }
        let mut forged = event.clone();
        let EventBody::BotCreated { instructions, .. } = &mut forged.body else {
            unreachable!()
        };
        *instructions = "Changed after the Account signed".into();
        let EventCause::Invocation { admission } = &mut forged.cause else {
            unreachable!()
        };
        admission.source_body["instructions"] = json!(instructions);
        admission.invocation.payload.body_hash = ObjectHash::body(&admission.source_body).unwrap();
        let facts = bot_creation_facts(&forged).unwrap();
        let EventCause::Invocation { admission } = &forged.cause else {
            unreachable!()
        };
        assert!(state.verify_original_admission(admission, &facts).is_err());
        assert!(
            validate_bot_definition(
                "bot",
                "chat",
                "name",
                "",
                Some(&BotProject {
                    target_node: node.node_id(),
                    cwd: "/workspace/../other".into(),
                })
            )
            .is_err()
        );
        let mut json = serde_json::to_value(&event).unwrap();
        json["body"]["membership_role_proof"] = json!({});
        assert!(serde_json::from_value::<Event>(json).is_err());
    }

    #[test]
    fn verifies_raw_event_signature_and_exact_envelope_context() {
        let (event, identity) = fixture();
        let envelope = sign_bytes(serde_json::to_string(&event).unwrap(), &identity);
        let verified = verify(&event, &envelope, &identity).unwrap();
        if let Ok(path) = std::env::var("VIBEFACE_EVENT_VECTOR_PATH") {
            std::fs::write(
                path,
                serde_json::to_vec(&serde_json::json!({
                    "envelope":envelope,"space":event.network_id,"resource":event.resource_id,
                    "key":identity.public_key_hex(),"context_hash":verified.context_hash(),
                }))
                .unwrap(),
            )
            .unwrap();
        }
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
