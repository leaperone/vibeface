use crate::model::*;
use anyhow::{Context as _, Result, ensure};
use ed25519_dalek::{Signature, Signer, SigningKey, VerifyingKey};
use rand::{RngCore, rngs::OsRng};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use zeroize::{Zeroize, ZeroizeOnDrop, Zeroizing};

const EVENT_DOMAIN: &[u8] = b"vibeface:event:v1:";
const PROVIDER_CONFIG_DOMAIN: &[u8] = b"vibeface:provider-runtime-key:v1";

#[derive(Clone, Serialize, Deserialize, Zeroize, ZeroizeOnDrop)]
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
        let mut seed = Zeroizing::new([0; 32]);
        OsRng.fill_bytes(&mut seed[..]);
        Self {
            secret_key_hex: hex::encode(&seed[..]),
        }
    }
    pub(crate) fn seed(&self) -> Result<Zeroizing<[u8; 32]>> {
        let mut seed = Zeroizing::new([0; 32]);
        hex::decode_to_slice(&self.secret_key_hex, &mut seed[..]).context("invalid device key")?;
        Ok(seed)
    }
    fn key(&self) -> Result<SigningKey> {
        let seed = self.seed()?;
        Ok(SigningKey::from_bytes(&seed))
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
    ) -> Result<vibeface_secure::Handshake> {
        let identity = vibeface_secure::Identity::from_secret_hex(&self.secret_key_hex)?;
        Ok(vibeface_secure::Handshake::signed(
            initiator,
            &identity,
            expected_device_id,
            context,
        )?)
    }
    pub fn sign_payload(&self, domain: &str, payload: &[u8]) -> Result<String> {
        Ok(
            vibeface_secure::Identity::from_secret_hex(&self.secret_key_hex)?
                .sign(domain, payload)?,
        )
    }

    pub(crate) fn provider_fingerprint(&self, payload: &[u8]) -> Result<String> {
        let seed = Zeroizing::new(self.key()?.to_bytes());
        let key = Zeroizing::new(hmac_sha256(&seed[..], PROVIDER_CONFIG_DOMAIN));
        Ok(hex::encode(hmac_sha256(&key[..], payload)))
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
        event.body = event.body.without_credentials()?;
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
    pub(crate) fn fixture_root(&self) -> Self {
        Self {
            secret_key_hex: digest(
                format!("vibeface:network-fixture-root:v1:{}", self.secret_key_hex).as_bytes(),
            ),
        }
    }
    /// The recovery key of `sign_network_fixture`'s network in `epoch`.
    #[cfg(test)]
    pub(crate) fn fixture_root_at(&self, epoch: u64) -> Self {
        match epoch {
            0 | 1 => self.fixture_root(),
            _ => Self {
                secret_key_hex: digest(
                    format!(
                        "vibeface:network-fixture-root:{epoch}:{}",
                        self.secret_key_hex
                    )
                    .as_bytes(),
                ),
            },
        }
    }
    #[cfg(test)]
    pub fn fixture_network_id(&self) -> Result<String> {
        self.fixture_root().device_id()
    }
    /// The client device in `sign_network_fixture`'s network.
    #[cfg(test)]
    pub fn fixture_admin(&self) -> Self {
        let mut admin_seed = b"vibeface:network-fixture-admin:v1:".to_vec();
        admin_seed.extend_from_slice(self.secret_key_hex.as_bytes());
        Self {
            secret_key_hex: hex::encode(Sha256::digest(admin_seed)),
        }
    }
    /// `body` with this client's signature, as a browser would send it.
    #[cfg(test)]
    pub fn attest(&self, network: &str, scope: &str, mut body: EventBody) -> Result<EventBody> {
        let signature = self.sign_payload(
            vibeface_secure::join::ATTESTATION_DOMAIN,
            &attestation_payload(network, scope, &body)?,
        )?;
        let value = Some(Attestation {
            device_id: self.device_id()?,
            signature,
        });
        match &mut body {
            EventBody::InputPosted { attestation, .. }
            | EventBody::WorkspaceCommanded { attestation, .. }
            | EventBody::ConversationOwner { attestation, .. }
            | EventBody::BotCreated { attestation, .. }
            | EventBody::BotDeleted { attestation, .. }
            | EventBody::MessagePosted { attestation, .. }
            | EventBody::WorkFinished { attestation, .. }
            | EventBody::NotificationAcked { attestation, .. }
            | EventBody::PrimaryBotSet { attestation, .. }
            | EventBody::BotProjectSet { attestation, .. }
            | EventBody::BotAvatarSet { attestation, .. }
            | EventBody::ScheduleControlled { attestation, .. }
            | EventBody::MemoryControlled { attestation, .. } => *attestation = value,
            _ => anyhow::bail!("this event carries no device signature"),
        }
        Ok(body)
    }
    /// The fixture client's signature making `node` the first owner of conversation `scope`.
    #[cfg(test)]
    pub fn owner_fixture(&self, network: &str, scope: &str, node: &str) -> Result<FirstOwner> {
        let body = EventBody::ConversationOwner {
            scheduler_node: node.into(),
            epoch: 1,
            attestation: None,
            cause: None,
        };
        let attestation = self
            .fixture_admin()
            .attest(network, scope, body)?
            .attestation()
            .cloned()
            .context("signed owner")?;
        Ok(FirstOwner {
            scheduler_node: node.into(),
            attestation,
        })
    }
    /// A recovered network whose members are a fixture admin client and `nodes`.
    #[cfg(test)]
    pub fn sign_network_fixture(
        &self,
        revision: u64,
        nodes: Vec<DeviceId>,
    ) -> Result<crate::identity::NetworkState> {
        use crate::identity::{Genesis, IdentityKind, Member, NetworkState, RootCheckpoint};

        let admin = self.fixture_admin();
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
        for epoch in 2..=revision {
            let next = self.fixture_root_at(epoch).device_id()?;
            let signer = self.fixture_root_at(epoch - 1);
            state.recover(RootCheckpoint::rotation(&state, &signer, &next, 0)?)?;
        }
        let epoch = revision + 1;
        let next = self.fixture_root_at(epoch).device_id()?;
        let signer = self.fixture_root_at(epoch - 1);
        state.recover(RootCheckpoint::recovery(
            &state,
            &signer,
            &next,
            members,
            vec![],
            0,
        )?)?;
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

pub fn random_hex(bytes: usize) -> String {
    let mut data = vec![0_u8; bytes];
    OsRng.fill_bytes(&mut data);
    hex::encode(data)
}

pub fn digest(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

pub(crate) fn hmac_sha256(key: &[u8], message: &[u8]) -> [u8; 32] {
    let mut block = [0u8; 64];
    if key.len() > 64 {
        block[..32].copy_from_slice(&Sha256::digest(key));
    } else {
        block[..key.len()].copy_from_slice(key);
    }
    let pad = |byte: u8| block.map(|b| b ^ byte);
    let inner = Sha256::new()
        .chain_update(pad(0x36))
        .chain_update(message)
        .finalize();
    Sha256::new()
        .chain_update(pad(0x5c))
        .chain_update(inner)
        .finalize()
        .into()
}
pub(crate) fn decode32(value: &str) -> Result<[u8; 32]> {
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
fn verify_signed_event(network_id: &str, event: &SignedEvent) -> Result<Event> {
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
    Ok(vibeface_secure::verify(
        public_key, domain, payload, signature,
    )?)
}
/// What a client signs for `body` (its `attestation` left out) in one conversation.
pub fn attestation_payload(network_id: &str, scope_id: &str, body: &EventBody) -> Result<Vec<u8>> {
    let mut value = serde_json::to_value(body)?;
    if let Some(object) = value.as_object_mut() {
        object.remove("attestation");
    }
    Ok(vibeface_secure::join::attestation_payload(
        network_id,
        scope_id,
        &value.to_string(),
    )?)
}

/// A logical signed operation keeps its identity across entry Node envelopes.
pub fn request_key(network: &str, scope: &str, body: &EventBody, author: &str) -> Result<String> {
    let signer = body
        .attestation()
        .map_or(author, |attestation| attestation.device_id.as_str());
    let mut signed = attestation_payload(network, scope, body)?;
    signed.extend_from_slice(signer.as_bytes());
    Ok(digest(&signed))
}

/// The user's own writes must carry their device signature; nothing else may carry one.
enum UserSignature {
    Required,
    /// The user's settlement of an unverified work result, or an answer.
    Optional,
    Forbidden,
}

fn validate_attestation(event: &Event) -> Result<()> {
    let rule = match &event.body {
        EventBody::InputPosted { .. } => UserSignature::Required,
        EventBody::WorkspaceCommanded {
            command: crate::workspace::WorkspaceCommand::AssignmentProgress { .. },
            ..
        }
        | EventBody::WorkspaceCommanded { cause: Some(_), .. } => UserSignature::Forbidden,
        EventBody::WorkspaceCommanded { .. } => UserSignature::Required,
        // The web signs an answer; one that comes through a decision token is the Node's own.
        EventBody::MessagePosted {
            role: Role::User,
            answers: Some(_),
            ..
        } => UserSignature::Optional,
        // A bot created in another bot's turn stands on the user's message; see `store::log`.
        EventBody::ConversationOwner { cause: Some(_), .. }
        | EventBody::BotCreated { cause: Some(_), .. }
        | EventBody::BotDeleted { cause: Some(_), .. } => UserSignature::Forbidden,
        EventBody::ConversationOwner { .. }
        | EventBody::BotCreated { .. }
        | EventBody::BotDeleted { .. }
        | EventBody::NotificationAcked { .. }
        | EventBody::PrimaryBotSet { .. }
        | EventBody::BotProjectSet { .. }
        | EventBody::BotAvatarSet { .. }
        | EventBody::ScheduleControlled { .. }
        | EventBody::MemoryControlled { .. } => UserSignature::Required,
        EventBody::WorkFinished {
            outcome: WorkOutcome::Completed | WorkOutcome::Interrupted,
            ..
        } => UserSignature::Optional,
        // It stands on the user's signed message among its parents; see `store::log`.
        EventBody::BotChanged { .. } => UserSignature::Forbidden,
        _ => UserSignature::Forbidden,
    };
    match (rule, event.body.attestation()) {
        (UserSignature::Required, None) => {
            anyhow::bail!("this write needs the user's device signature")
        }
        (UserSignature::Forbidden, Some(_)) => {
            anyhow::bail!("only the user's own writes carry a device signature")
        }
        (_, None) => Ok(()),
        _ => verify_attestation(&event.network_id, &event.scope_id, &event.body),
    }
}

pub fn verify_attestation(network_id: &str, scope_id: &str, body: &EventBody) -> Result<()> {
    let attestation = body.attestation().context("missing device signature")?;
    verify_payload(
        &attestation.device_id,
        vibeface_secure::join::ATTESTATION_DOMAIN,
        &attestation_payload(network_id, scope_id, body)?,
        &attestation.signature,
    )
    .context("the device signature does not match this write")
}

/// Bots' questions start with `q-`.
fn question_id(question: &str) -> Result<()> {
    vibeface_protocol::validate_id(question).map_err(anyhow::Error::msg)?;
    ensure!(question.starts_with("q-"), "a question ID starts with q-");
    Ok(())
}

/// A bot's turn creates another conversation; the cause sits in the creating one.
fn cause_id(scope: &str, cause: &Cause) -> Result<()> {
    vibeface_protocol::validate_id(&cause.conversation_id).map_err(anyhow::Error::msg)?;
    vibeface_protocol::validate_id(&cause.message_id).map_err(anyhow::Error::msg)?;
    ensure!(
        cause.conversation_id != scope,
        "a bot creates or deletes only another bot's conversation"
    );
    Ok(())
}

fn subject(node: &str, session: &str, cwd: &str, summary: &str) -> Result<()> {
    vibeface_secure::validate_device_id(node)?;
    ensure!(
        !session.is_empty() && session.len() <= 256 && cwd.len() <= 4096 && summary.len() <= 1024,
        "invalid session subject"
    );
    Ok(())
}

pub fn validate_event_shape(event: &Event) -> Result<()> {
    ensure!(event.version == WIRE_VERSION, "unsupported event version");
    ensure!(
        !event.scope_id.is_empty() && event.scope_id.len() <= 512,
        "invalid scope"
    );
    vibeface_secure::membership::validate_hash(&event.network_id)?;
    vibeface_secure::validate_device_id(&event.author)?;
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
        vibeface_secure::membership::validate_hash(hash)?;
    }
    ensure!(
        event
            .prev
            .as_ref()
            .is_none_or(|prev| !event.deps.contains(prev)),
        "prev is not a dependency on another author"
    );
    for blob in &event.blobs {
        vibeface_secure::membership::validate_hash(&blob.hash)?;
        ensure!(blob.size <= MAX_BLOB_BYTES as u64, "blob too large");
    }
    if let Some(attestation) = event.body.attestation() {
        vibeface_secure::validate_device_id(&attestation.device_id)?;
    }
    if let EventBody::BotCreated {
        project: Some(project),
        ..
    }
    | EventBody::BotProjectSet {
        project: Some(project),
        ..
    } = &event.body
    {
        vibeface_secure::validate_device_id(&project.target_node)?;
    }
    let id = |value: &str| vibeface_protocol::validate_id(value).map_err(anyhow::Error::msg);
    match &event.body {
        EventBody::InputPosted {
            message_id,
            target,
            sequence,
            expected_revision,
            text,
            attachments,
            report_results,
            ..
        } => {
            id(message_id)?;
            crate::store::workspace::validate_target(target)?;
            ensure!(
                *sequence > 0 && *sequence <= i64::MAX as u64,
                "invalid input sequence"
            );
            ensure!(
                expected_revision.is_none_or(|revision| revision > 0),
                "invalid input revision"
            );
            ensure!(
                text.len() <= 65536 && (!text.trim().is_empty() || !attachments.is_empty()),
                "invalid input text"
            );
            ensure!(
                report_results.is_some(),
                "an input carries its reporting choice"
            );
            crate::store::workspace::validate_attachments(attachments)?;
            ensure!(
                attachments
                    .iter()
                    .map(Attachment::blob)
                    .eq(event.blobs.iter().cloned()),
                "input attachments do not match event blobs"
            );
        }
        EventBody::WorkspaceCommanded {
            command_id,
            command,
            acting_assignment,
            attestation,
            cause,
        } => {
            if let Some(id) = acting_assignment {
                vibeface_protocol::validate_id(id).map_err(anyhow::Error::msg)?;
            }
            ensure!(
                acting_assignment.is_none() || (attestation.is_none() && cause.is_some()),
                "only a Bot's command has an acting assignment"
            );
            id(command_id)?;
            ensure!(
                event.scope_id == crate::workspace::WORKSPACE_SCOPE,
                "invalid workspace command scope"
            );
            ensure!(
                !(attestation.is_some() && cause.is_some()),
                "a command has one authority"
            );
            crate::store::workspace::validate_command(command)?;
            if let Some(cause) = cause {
                id(&cause.conversation_id)?;
                id(&cause.message_id)?;
            }
            let attachments = match command {
                crate::workspace::WorkspaceCommand::PostChannelMessage { attachments, .. }
                | crate::workspace::WorkspaceCommand::ControlInput {
                    action: crate::workspace::InputControl::Edit { attachments, .. },
                    ..
                } => attachments.as_slice(),
                _ => &[],
            };
            ensure!(
                attachments
                    .iter()
                    .map(Attachment::blob)
                    .eq(event.blobs.iter().cloned()),
                "workspace attachments do not match event blobs"
            );
        }
        EventBody::MessagePosted {
            message_id,
            role,
            attachments,
            report_results,
            in_reply_to,
            status,
            answers,
            ..
        } => {
            id(message_id)?;
            match role {
                Role::User => {
                    let question = answers
                        .as_ref()
                        .context("ordinary user messages use InputPosted")?;
                    question_id(question)?;
                    ensure!(
                        *message_id == format!("a-{question}"),
                        "an answer's message ID is derived from its question"
                    );
                    ensure!(
                        report_results.is_some()
                            && in_reply_to.is_none()
                            && status.is_none()
                            && attachments
                                .iter()
                                .map(Attachment::blob)
                                .eq(event.blobs.iter().cloned()),
                        "a user message carries only its reporting choice and its signed attachments"
                    );
                    ensure!(
                        attachments
                            .windows(2)
                            .all(|pair| pair[0].hash < pair[1].hash)
                            && attachments
                                .iter()
                                .all(|attachment| valid_attachment_name(&attachment.name)),
                        "attachments are ordered by content, once each, with clean file names"
                    );
                }
                Role::Assistant => {
                    ensure!(
                        report_results.is_none()
                            && status.is_some()
                            && attachments.is_empty()
                            && answers.is_none(),
                        "a bot reply carries its turn status"
                    );
                    if let Some(answered) = in_reply_to
                        && crate::scheduling::parse_schedule_trigger(answered).is_none()
                    {
                        id(answered)?;
                    }
                }
                Role::Bot => anyhow::bail!("bots message each other through their own event"),
            }
        }
        EventBody::ConversationOwner {
            scheduler_node,
            epoch,
            cause,
            ..
        } => {
            vibeface_secure::validate_device_id(scheduler_node)?;
            ensure!(*epoch > 0, "owner epoch starts at 1");
            if let Some(cause) = cause {
                ensure!(
                    *epoch == 1,
                    "only a created bot's first owner stands on a cause"
                );
                cause_id(&event.scope_id, cause)?;
            }
        }
        EventBody::BotCreated {
            cause: Some(cause), ..
        } => cause_id(&event.scope_id, cause)?,
        EventBody::BotDeleted { cause, .. } => {
            ensure!(
                event.scope_id != BOTS_SCOPE,
                "a bot is deleted in its own conversation"
            );
            cause
                .as_ref()
                .map(|cause| cause_id(&event.scope_id, cause))
                .transpose()?;
        }
        EventBody::WorkDispatched {
            work_id,
            trigger,
            dispatcher,
            call_ref,
            kind,
            model,
            resolved,
            continues,
            parent,
            thread,
            ..
        } => {
            id(work_id)?;
            match trigger.split_once(':').unwrap_or_default() {
                ("message" | "work" | "task" | "assignment" | "bot_message", trigger_id) => {
                    id(trigger_id)?
                }
                ("schedule", _) => ensure!(
                    crate::scheduling::parse_schedule_trigger(trigger).is_some(),
                    "invalid schedule trigger"
                ),
                _ => anyhow::bail!("invalid work trigger"),
            }
            id(call_ref)?;
            vibeface_secure::validate_device_id(dispatcher)?;
            if let Some(model) = model {
                crate::model::validate_model(model)?;
            }
            let role = if *kind == WorkKind::Task {
                crate::providers::Role::Agent
            } else {
                crate::providers::Role::Execution
            };
            if let WorkKind::Agent(provider) = kind {
                ensure!(
                    resolved.provider == *provider,
                    "resolved work provider does not match its agent"
                );
            }
            ensure!(
                resolved.provider.supports().contains(&role),
                "resolved work provider cannot run this kind"
            );
            if let Some(model) = &resolved.model {
                crate::model::validate_model(model)?;
            }
            for work in continues.iter().chain(parent.iter()).chain(thread.iter()) {
                id(work)?;
            }
        }
        EventBody::WorkProgress { work_id, .. } | EventBody::WorkFinished { work_id, .. } => {
            id(work_id)?
        }
        EventBody::ConversationCreated { .. } => ensure!(
            event.scope_id != BOTS_SCOPE,
            "the bots scope is not a conversation"
        ),
        EventBody::BotProjectSet { .. } => ensure!(
            event.scope_id != BOTS_SCOPE,
            "a bot's project is set in its own conversation"
        ),
        EventBody::BotAvatarSet { version, .. } => {
            ensure!(
                event.scope_id != BOTS_SCOPE,
                "a bot's face is set in its own conversation"
            );
            ensure!(
                version.as_ref().is_none_or(|version| version.len() == 16
                    && version
                        .bytes()
                        .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))),
                "a face version is the start of an event hash"
            );
        }
        EventBody::PrimaryBotSet {
            conversation_id, ..
        } => {
            ensure!(
                event.scope_id == BOTS_SCOPE,
                "the primary bot is set in the bots scope"
            );
            id(conversation_id)?;
        }
        EventBody::QuestionAsked {
            question_id: question,
            target,
            attention,
            text,
            choices,
            about,
            ..
        } => {
            question_id(question)?;
            if let Some(target) = target {
                crate::store::workspace::validate_target(target)?;
            }
            ensure!(
                !text.trim().is_empty() && text.len() <= 1024,
                "a question is 1-1024 bytes"
            );
            subject(&about.node, &about.session_id, &about.cwd, &about.summary)?;
            about.request_id.as_deref().map(id).transpose()?;
            ensure!(
                attention
                    .device_id
                    .as_ref()
                    .is_none_or(|device| device == &about.node),
                "attention must target the request's device"
            );
            ensure!(
                attention.kind != crate::model::AttentionKind::BrowserTakeover
                    || attention.device_id.is_some(),
                "browser takeover requires its running device"
            );
            ensure!(choices.len() <= 8, "at most 8 choices");
            for (index, choice) in choices.iter().enumerate() {
                ensure!(
                    !choice.label.trim().is_empty()
                        && choice.label.chars().count() <= 120
                        && choices[..index].iter().all(|c| c.label != choice.label),
                    "choice labels are 1-120 characters and unique"
                );
            }
        }
        EventBody::QuestionClosed {
            question_id: question,
            ..
        } => {
            question_id(question)?;
            ensure!(
                event.scope_id != BOTS_SCOPE,
                "a question is closed in its conversation"
            );
        }
        EventBody::SessionAttributed {
            node,
            session_id,
            cwd,
            summary,
            ..
        } => {
            ensure!(
                event.scope_id != BOTS_SCOPE,
                "a session is attributed in its bot's conversation"
            );
            ensure!(
                *node == event.author,
                "only the device running a session hands it over"
            );
            subject(node, session_id, cwd, summary)?;
        }
        EventBody::SessionMuted { node, session_id } => {
            ensure!(
                event.scope_id != BOTS_SCOPE,
                "a session is muted in its bot's conversation"
            );
            ensure!(
                *node == event.author,
                "only the device running a session mutes it"
            );
            subject(node, session_id, "", "")?;
        }
        EventBody::ContextCheckpoint { covers, .. } => {
            for author in covers.keys() {
                vibeface_secure::validate_device_id(author)?;
            }
        }
        EventBody::BotChanged {
            change_id,
            cause,
            change,
        } => {
            id(change_id)?;
            if let Some(cause) = cause {
                id(&cause.conversation_id)?;
                id(&cause.message_id)?;
            }
            change.validate()?;
        }
        EventBody::ScheduleControlled { schedule_id, .. } => id(schedule_id)?,
        EventBody::MemoryControlled {
            memory_id,
            base,
            after,
            ..
        } => {
            id(memory_id)?;
            ensure!(
                event.scope_id != BOTS_SCOPE,
                "a memory belongs to its bot's conversation"
            );
            base.validate()?;
            base.next_control_revision()?;
            ensure!(
                after.is_some() || base.version.is_some(),
                "a memory deletion needs a base"
            );
            after.as_deref().map(validate_memory).transpose()?;
        }
        EventBody::MessageSteered { message_id, reply } => {
            id(message_id)?;
            id(reply)?;
            ensure!(
                event.scope_id != BOTS_SCOPE,
                "a message is steered in its bot's conversation"
            );
        }
        EventBody::ScheduleRunClaimed { schedule_id, .. } => {
            id(schedule_id)?;
            ensure!(
                event.scope_id != BOTS_SCOPE,
                "a schedule runs in its bot's conversation"
            );
        }
        EventBody::BotMessaged {
            message_id,
            from,
            text,
            hops,
        } => {
            id(message_id)?;
            id(&from.conversation_id)?;
            ensure!(
                from.conversation_id != event.scope_id && event.scope_id != BOTS_SCOPE,
                "a bot messages another bot's conversation"
            );
            ensure!(
                from.name.chars().count() <= crate::model::MAX_SCHEDULE_NAME_CHARS
                    && !from.name.contains('\n'),
                "a sender name is one short line"
            );
            ensure!(
                !text.trim().is_empty() && text.len() <= crate::model::MAX_BOT_MESSAGE_BYTES,
                "a bot message is 1-{} bytes",
                crate::model::MAX_BOT_MESSAGE_BYTES
            );
            ensure!(
                (1..=crate::model::MAX_CHAIN_DEPTH).contains(hops),
                "a bot message is at most {} hops deep",
                crate::model::MAX_CHAIN_DEPTH
            );
        }
        _ => {}
    }
    validate_attestation(event)
}

#[cfg(test)]
mod tests {

    use super::*;
    use crate::providers::Provider;
    #[test]
    fn hmac_matches_rfc_4231() {
        assert_eq!(
            hex::encode(hmac_sha256(&[0x0b; 20], b"Hi There")),
            "b0344c61d8db38535ca8afceaf0bf12b881dc200c9833da726e9376c2e32cff7"
        );
        assert_eq!(
            hex::encode(hmac_sha256(
                &[0xaa; 131],
                b"Test Using Larger Than Block-Size Key - Hash Key First"
            )),
            "60e431591ee0b67f0d8a26aacbf5b77f8e0bc6213728c5140546040f0ee37f54"
        );
    }

    #[test]
    fn provider_fingerprints_are_stable_and_private_to_the_device() -> Result<()> {
        let identity = LocalIdentity {
            secret_key_hex: hex::encode([7; 32]),
        };
        let payload = b"provider-config-fixture";
        let tag = identity.provider_fingerprint(payload)?;
        assert_eq!(
            tag,
            "19cdf6bf40dd7beddd0fcb28b93d1ce8450c62da516aa4d1147c971fcc47d714"
        );
        let encoded = Zeroizing::new(serde_json::to_vec(&identity)?);
        let restored: LocalIdentity = serde_json::from_slice(&encoded)?;
        assert_eq!(restored.provider_fingerprint(payload)?, tag);
        let mut cleared = restored.clone();
        cleared.zeroize();
        assert!(cleared.secret_key_hex.is_empty());
        assert_eq!(restored.provider_fingerprint(payload)?, tag);
        let uppercase = LocalIdentity {
            secret_key_hex: "AB".repeat(32),
        };
        let lowercase = LocalIdentity {
            secret_key_hex: "ab".repeat(32),
        };
        assert_eq!(uppercase.device_id()?, lowercase.device_id()?);
        assert_eq!(decode32(&uppercase.secret_key_hex)?, [0xab; 32]);
        assert_ne!(identity.provider_fingerprint(b"changed-config")?, tag);
        assert_ne!(
            LocalIdentity {
                secret_key_hex: hex::encode([8; 32]),
            }
            .provider_fingerprint(payload)?,
            tag
        );
        assert!(
            LocalIdentity {
                secret_key_hex: "invalid".into(),
            }
            .provider_fingerprint(payload)
            .is_err()
        );
        Ok(())
    }

    #[test]
    fn memory_request_key_matches_the_shared_client_vector() -> Result<()> {
        let network = "a".repeat(64);
        let body = EventBody::MemoryControlled {
            memory_id: "memory".into(),
            base: MemoryBase {
                epoch: crate::config::DATA_EPOCH,
                version: None,
            },
            after: Some("用中文".into()),
            attestation: Some(Attestation {
                device_id: "b".repeat(64),
                signature: String::new(),
            }),
        };
        assert_eq!(
            request_key(&network, "chat", &body, "")?,
            "d92a5487e2ff1845032721e6af4d56c32b40fd36d79fa93046887c2f4a48059f"
        );
        Ok(())
    }

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
        let original: Event = serde_json::from_str(&event.canonical_body)?;
        for field in ["author", "network_id", "deps", "blobs", "covers"] {
            let mut bad = original.clone();
            match field {
                "author" => bad.author.make_ascii_uppercase(),
                "network_id" => bad.network_id.make_ascii_uppercase(),
                "deps" => bad.deps = vec![digest(b"dependency").to_uppercase()],
                "blobs" => {
                    bad.blobs = vec![BlobRef {
                        hash: digest(b"blob").to_uppercase(),
                        size: 0,
                    }];
                }
                "covers" => {
                    bad.body = EventBody::ContextCheckpoint {
                        covers: VersionVector::from([(original.author.to_uppercase(), 1)]),
                        summary: String::new(),
                        recent_user_requests: vec![],
                        open_work: vec![],
                        decisions: vec![],
                    };
                }
                _ => unreachable!(),
            }
            let canonical_body = serde_json::to_string(&bad)?;
            let envelope = SignedEvent {
                hash: digest(canonical_body.as_bytes()),
                canonical_body,
                signature: event.signature.clone(),
            };
            assert!(check_event(&view.network_id, &envelope).is_err(), "{field}");
        }
        let mut bad_prev = original.clone();
        bad_prev.seq = 2;
        bad_prev.prev = Some(digest(b"previous").to_uppercase());
        assert!(validate_event_shape(&bad_prev).is_err());
        let mut weak = original.clone();
        weak.author = "00".repeat(32);
        assert!(validate_event_shape(&weak).is_err());
        let mut base = MemoryBase {
            epoch: crate::config::DATA_EPOCH,
            version: Some(MemoryVersion {
                revision: digest(b"revision"),
                control_revision: 0,
                source_event_hash: digest(b"source"),
            }),
        };
        base.validate()?;
        base.version
            .as_mut()
            .unwrap()
            .revision
            .make_ascii_uppercase();
        assert!(base.validate().is_err());
        assert_eq!(serde_json::to_string(&original)?, event.canonical_body);
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
    fn user_attachments_are_named_ordered_and_match_the_event_blobs() -> Result<()> {
        let identity = LocalIdentity::generate();
        let network = identity.fixture_network_id()?;
        let client = identity.fixture_admin();
        let file = |seed: &[u8], name: &str| Attachment {
            hash: digest(seed),
            size: 3,
            name: name.into(),
        };
        let mut files = vec![file(b"a", "报告 v2.pdf"), file(b"b", "notes.txt")];
        files.sort();
        let sign = |attachments: Vec<Attachment>, blobs: Vec<BlobRef>| -> Result<SignedEvent> {
            let body = client.attest(
                &network,
                "conversation",
                EventBody::InputPosted {
                    message_id: "m-1".into(),
                    target: crate::workspace::InputTarget::BotMain {
                        bot: "conversation".into(),
                    },
                    intent: crate::workspace::InputIntent::Continue,
                    sequence: 1,
                    expected_revision: None,
                    text: "see files".into(),
                    attachments,
                    report_results: Some(true),
                    attestation: None,
                },
            )?;
            identity.sign_event(Event {
                version: WIRE_VERSION,
                network_id: network.clone(),
                author: identity.device_id()?,
                seq: 1,
                prev: None,
                deps: vec![],
                scope_id: "conversation".into(),
                created_at_ms: 0,
                body,
                blobs,
            })
        };
        let blobs = |files: &[Attachment]| files.iter().map(Attachment::blob).collect::<Vec<_>>();
        let signed = sign(files.clone(), blobs(&files))?;
        match verify_signed_event(&network, &signed)?.body {
            EventBody::InputPosted { attachments, .. } => assert_eq!(attachments, files),
            _ => unreachable!(),
        }
        assert!(
            sign(files.clone(), blobs(&files[..1])).is_err(),
            "every attachment's content travels with the event"
        );
        let reversed: Vec<_> = files.iter().rev().cloned().collect();
        assert!(sign(reversed.clone(), blobs(&reversed)).is_err());
        let twice = vec![files[0].clone(), files[0].clone()];
        assert!(sign(twice.clone(), blobs(&twice)).is_err());
        for name in ["", "../etc/passwd", "a\\b", "bell\u{7}", &"x".repeat(256)] {
            let bad = vec![file(b"a", name)];
            assert!(
                sign(bad.clone(), blobs(&bad)).is_err(),
                "{name:?} is not a clean name"
            );
        }
        let longest = vec![file(b"a", &"名".repeat(85))];
        assert!(sign(longest.clone(), blobs(&longest)).is_ok());
        Ok(())
    }

    #[test]
    fn message_roles_and_log_links_are_part_of_the_signed_shape() -> Result<()> {
        let identity = LocalIdentity::generate();
        let message = |role, report_results, status| EventBody::MessagePosted {
            message_id: "m-1".into(),
            role,
            text: "hi".into(),
            attachments: vec![],
            report_results,
            in_reply_to: None,
            status,
            answers: None,
            attestation: None,
            failure: None,
        };
        let network = identity.fixture_network_id()?;
        let client = identity.fixture_admin();
        let input = || EventBody::InputPosted {
            message_id: "m-1".into(),
            target: crate::workspace::InputTarget::BotMain {
                bot: "conversation".into(),
            },
            intent: crate::workspace::InputIntent::Continue,
            sequence: 1,
            expected_revision: None,
            text: "hi".into(),
            attachments: vec![],
            report_results: Some(true),
            attestation: None,
        };
        let signed_user = client.attest(&network, "conversation", input())?;
        let mut event = Event {
            version: WIRE_VERSION,
            network_id: network.clone(),
            author: identity.device_id()?,
            seq: 1,
            prev: None,
            deps: vec![digest(b"b"), digest(b"a"), digest(b"b")],
            scope_id: "conversation".into(),
            created_at_ms: 0,
            body: signed_user.clone(),
            blobs: vec![],
        };
        let signed = identity.sign_event(event.clone())?;
        let verified = verify_signed_event(&event.network_id, &signed)?;
        assert_eq!(verified.deps.len(), 2);
        assert!(verified.deps.windows(2).all(|pair| pair[0] < pair[1]));
        event.body = input();
        assert!(identity.sign_event(event.clone()).is_err());
        event.body = signed_user.clone();
        if let EventBody::InputPosted { text, .. } = &mut event.body {
            *text = "rm -rf".into();
        }
        assert!(identity.sign_event(event.clone()).is_err());
        event.body = signed_user;
        event.scope_id = "other".into();
        assert!(identity.sign_event(event.clone()).is_err());
        event.scope_id = "conversation".into();
        assert!(
            client
                .attest(
                    &network,
                    "conversation",
                    message(Role::Assistant, None, Some(ReplyStatus::Stopped))
                )
                .is_ok_and(|body| identity
                    .sign_event(Event {
                        body,
                        ..event.clone()
                    })
                    .is_err()),
            "bot replies carry no device signature"
        );
        event.body = message(Role::User, None, None);
        assert!(identity.sign_event(event.clone()).is_err());
        event.body = client.attest(
            &network,
            "conversation",
            message(Role::User, Some(true), None),
        )?;
        assert!(identity.sign_event(event.clone()).is_err());
        let answer = |id: &str, role, report_results, status| {
            let mut body = message(role, report_results, status);
            if let EventBody::MessagePosted {
                message_id,
                answers,
                ..
            } = &mut body
            {
                *message_id = id.into();
                *answers = Some("q-1".into());
            }
            body
        };
        event.body = answer("a-q-1", Role::User, Some(true), None);
        assert!(identity.sign_event(event.clone()).is_ok());
        event.body = answer("m-1", Role::User, Some(true), None);
        assert!(identity.sign_event(event.clone()).is_err());
        event.body = answer("a-q-1", Role::Assistant, None, Some(ReplyStatus::Stopped));
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

    #[test]
    fn frozen_agent_work_provider_matches_the_selected_executor() -> Result<()> {
        let identity = LocalIdentity::generate();
        let network = identity.fixture_network_id()?;
        let event = |provider| Event {
            version: WIRE_VERSION,
            network_id: network.clone(),
            scope_id: "conversation".into(),
            author: identity.device_id().unwrap(),
            seq: 1,
            prev: None,
            deps: vec![],
            created_at_ms: 0,
            body: EventBody::WorkDispatched {
                work_id: "work-1".into(),
                trigger: "message:m-1".into(),
                dispatcher: identity.device_id().unwrap(),
                call_ref: "call-1".into(),
                kind: WorkKind::Agent(Provider::Codex),
                model: None,
                resolved: ResolvedModel {
                    provider,
                    model: None,
                },
                cwd: "/tmp".into(),
                title: "run".into(),
                prompt: "run".into(),
                continues: None,
                parent: None,
                thread: None,
            },
            blobs: vec![],
        };
        identity
            .sign_event(event(Provider::Codex))
            .expect("matching frozen provider should be valid");
        let mut bad_dispatcher = event(Provider::Codex);
        if let EventBody::WorkDispatched { dispatcher, .. } = &mut bad_dispatcher.body {
            dispatcher.make_ascii_uppercase();
        }
        assert!(identity.sign_event(bad_dispatcher).is_err());
        let mut assigned = event(Provider::Codex);
        if let EventBody::WorkDispatched { trigger, .. } = &mut assigned.body {
            *trigger = "assignment:assignment-1".into();
        }
        identity.sign_event(assigned.clone())?;
        if let EventBody::WorkDispatched { trigger, .. } = &mut assigned.body {
            *trigger = "assignment:invalid/id".into();
        }
        assert!(identity.sign_event(assigned).is_err());
        assert!(identity.sign_event(event(Provider::Claude)).is_err());
        let mut missing = serde_json::to_value(event(Provider::Codex))?;
        missing["body"]
            .as_object_mut()
            .context("body")?
            .remove("resolved");
        assert!(serde_json::from_value::<Event>(missing).is_err());
        Ok(())
    }
}
