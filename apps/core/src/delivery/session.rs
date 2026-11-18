//! Safe event payloads for the encrypted session relay.
//!
//! This module only selects and serializes non-secret event data. The returned
//! bytes are plaintext input for a caller-owned encryption step.

use crate::model::EventBody;
use anyhow::{Result, bail};
use vibeface_delivery::{
    EncryptedSessionObject, MAX_SESSION_OBJECT_BYTES, SessionObjectKind, Word,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionSignatureVerification {
    Verified,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionIdempotency {
    New,
    AlreadySeen,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InboundSessionReceipt {
    pub object_id: Word,
    pub kind: SessionObjectKind,
    pub conversation_id: String,
    pub sender_peer: Word,
    pub receiver_peer: Word,
    pub payload_hash: Word,
    pub signature_verification: SessionSignatureVerification,
    pub received_at_ms: u64,
    pub idempotency: SessionIdempotency,
}

/// Classify the event kinds that may cross an encrypted session relay.
pub fn kind_for_event(event: &EventBody) -> Result<SessionObjectKind> {
    match event {
        EventBody::MessagePosted { .. }
        | EventBody::InputPosted { .. }
        | EventBody::BotMessaged { .. } => Ok(SessionObjectKind::Message),
        EventBody::BotCreated { .. }
        | EventBody::BotChanged { .. }
        | EventBody::BotAvatarSet { .. }
        | EventBody::BotProjectSet { .. }
        | EventBody::BotDeleted { .. }
        | EventBody::ScheduleControlled { .. }
        | EventBody::MemoryControlled { .. } => Ok(SessionObjectKind::BotSettings),
        EventBody::ConversationOwner { .. }
        | EventBody::SessionAttributed { .. }
        | EventBody::SessionMuted { .. }
        | EventBody::ScheduleRunClaimed { .. }
        | EventBody::ContextCheckpoint { .. } => Ok(SessionObjectKind::ConversationState),
        _ => bail!("event kind is not supported by the encrypted session relay"),
    }
}

/// Serialize a relay-safe event after removing credentials known to this Node.
///
/// The result is plaintext bytes for the encryption layer. This function does
/// not write the event, encrypt it, or trigger the Bot scheduler.
pub fn event_payload(event: &EventBody) -> Result<Vec<u8>> {
    kind_for_event(event)?;
    let safe = event.clone().without_credentials()?;
    Ok(serde_json::to_vec(&safe)?)
}

/// Return the payload kind and canonical JSON bytes for a relay object.
pub fn event_object_payload(event: &EventBody) -> Result<(SessionObjectKind, Vec<u8>)> {
    Ok((kind_for_event(event)?, event_payload(event)?))
}

/// Decode a previously decrypted relay payload and enforce its declared class.
///
/// This is still a pure boundary check. It does not persist the event or run
/// the Bot scheduler.
pub fn decode_event_payload(expected_kind: SessionObjectKind, payload: &[u8]) -> Result<EventBody> {
    if payload.is_empty() || payload.len() > MAX_SESSION_OBJECT_BYTES {
        bail!("encrypted session event payload size is invalid");
    }
    let event: EventBody = serde_json::from_slice(payload)?;
    let actual_kind = kind_for_event(&event)?;
    if actual_kind != expected_kind {
        bail!("encrypted session event kind does not match its envelope");
    }
    Ok(event)
}

/// Verify and classify one decrypted inbound object without writing it anywhere.
pub fn accept_inbound_session_object(
    object: &EncryptedSessionObject,
    expected_kind: SessionObjectKind,
    decrypted_payload: &[u8],
    sender_peer: &Word,
    domain: &str,
    chunk_bytes: usize,
    received_at_ms: u64,
    seen_object_ids: &[Word],
) -> Result<InboundSessionReceipt> {
    anyhow::ensure!(
        object.header.sender_peer == *sender_peer,
        "session object sender does not match the verifying peer"
    );
    anyhow::ensure!(
        object.header.kind == expected_kind,
        "session object kind does not match the expected payload kind"
    );
    object.validate(chunk_bytes)?;
    object.verify_signature(sender_peer, domain, chunk_bytes)?;
    decode_event_payload(expected_kind, decrypted_payload)?;
    let idempotency = if seen_object_ids
        .iter()
        .any(|seen| *seen == object.header.object_id)
    {
        SessionIdempotency::AlreadySeen
    } else {
        SessionIdempotency::New
    };
    Ok(InboundSessionReceipt {
        object_id: object.header.object_id,
        kind: object.header.kind,
        conversation_id: object.header.conversation_id.clone(),
        sender_peer: object.header.sender_peer,
        receiver_peer: object.header.receiver_peer,
        payload_hash: object.header.payload_hash,
        signature_verification: SessionSignatureVerification::Verified,
        received_at_ms,
        idempotency,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{EventBody, Role};
    use ed25519_dalek::{Signer, SigningKey};

    fn message() -> EventBody {
        EventBody::MessagePosted {
            message_id: "message-1".into(),
            role: Role::User,
            text: "encrypted request".into(),
            attachments: vec![],
            report_results: None,
            in_reply_to: None,
            status: None,
            failure: None,
            answers: None,
            attestation: None,
        }
    }

    #[test]
    fn classifies_messages_and_serializes_payload_without_side_effects() {
        assert_eq!(
            kind_for_event(&message()).unwrap(),
            SessionObjectKind::Message
        );
        let bytes = event_payload(&message()).unwrap();
        let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(value["type"], "message_posted");
        assert_eq!(value["text"], "encrypted request");
    }

    #[test]
    fn classifies_context_checkpoints_as_session_state() {
        let event = EventBody::ContextCheckpoint {
            covers: Default::default(),
            summary: "state".into(),
            recent_user_requests: vec![],
            open_work: vec![],
            decisions: vec![],
        };
        assert_eq!(
            kind_for_event(&event).unwrap(),
            SessionObjectKind::ConversationState
        );
        assert!(!event_payload(&event).unwrap().is_empty());
    }

    #[test]
    fn rejects_events_that_are_not_session_payloads() {
        let event = EventBody::ConversationCreated {
            title: "chat".into(),
        };
        assert!(kind_for_event(&event).is_err());
        assert!(event_payload(&event).is_err());
    }

    #[test]
    fn classifies_bot_messages_as_messages() {
        let event = EventBody::BotMessaged {
            message_id: "bot-message-1".into(),
            from: crate::model::Sender {
                conversation_id: "source".into(),
                name: "Helper".into(),
            },
            text: "handoff".into(),
            hops: 0,
        };
        assert_eq!(kind_for_event(&event).unwrap(), SessionObjectKind::Message);
        assert!(!event_payload(&event).unwrap().is_empty());
    }

    #[test]
    fn decodes_message_and_bot_message_payloads_at_the_boundary() {
        let message = message();
        let decoded = decode_event_payload(
            SessionObjectKind::Message,
            &event_payload(&message).unwrap(),
        )
        .unwrap();
        assert_eq!(decoded, message);

        let bot = EventBody::BotMessaged {
            message_id: "bot-message-1".into(),
            from: crate::model::Sender {
                conversation_id: "source".into(),
                name: "Helper".into(),
            },
            text: "handoff".into(),
            hops: 0,
        };
        let decoded =
            decode_event_payload(SessionObjectKind::Message, &event_payload(&bot).unwrap())
                .unwrap();
        assert_eq!(decoded, bot);
    }

    #[test]
    fn decode_rejects_mismatch_malformed_and_unsupported_payloads() {
        let bytes = event_payload(&message()).unwrap();
        assert!(decode_event_payload(SessionObjectKind::BotSettings, &bytes).is_err());
        assert!(decode_event_payload(SessionObjectKind::Message, b"not json").is_err());
        let unsupported = serde_json::to_vec(&EventBody::ConversationCreated {
            title: "chat".into(),
        })
        .unwrap();
        assert!(decode_event_payload(SessionObjectKind::ConversationState, &unsupported).is_err());
        assert!(decode_event_payload(SessionObjectKind::Message, &[]).is_err());
    }

    fn signed_message_object() -> (EncryptedSessionObject, EventBody, Word) {
        let event = message();
        let payload = event_payload(&event).unwrap();
        let key = SigningKey::from_bytes(&[7; 32]);
        let sender = key.verifying_key().to_bytes();
        let object = EncryptedSessionObject::new(
            SessionObjectKind::Message,
            "network-1".into(),
            "conversation-1".into(),
            0,
            [0; 32],
            sender,
            [2; 32],
            [3; 32],
            100,
            200,
            payload,
            32,
        )
        .unwrap();
        let signature = key.sign(
            &vibeface_delivery::signing_message(
                "session-object",
                &object.signing_bytes(32).unwrap(),
            )
            .unwrap(),
        );
        (
            object.with_signature(vibeface_delivery::PeerSignature::from_bytes(
                signature.to_bytes(),
            )),
            event,
            sender,
        )
    }

    #[test]
    fn accepts_new_and_repeated_inbound_objects_idempotently() {
        let (object, event, sender) = signed_message_object();
        let payload = event_payload(&event).unwrap();
        let first = accept_inbound_session_object(
            &object,
            SessionObjectKind::Message,
            &payload,
            &sender,
            "session-object",
            32,
            300,
            &[],
        )
        .unwrap();
        assert_eq!(
            first.signature_verification,
            SessionSignatureVerification::Verified
        );
        assert_eq!(first.idempotency, SessionIdempotency::New);
        let retry = accept_inbound_session_object(
            &object,
            SessionObjectKind::Message,
            &payload,
            &sender,
            "session-object",
            32,
            301,
            &[object.header.object_id],
        )
        .unwrap();
        assert_eq!(retry.idempotency, SessionIdempotency::AlreadySeen);
        assert_eq!(retry.object_id, first.object_id);
    }

    #[test]
    fn rejects_inbound_objects_with_bad_signature_or_payload_kind() {
        let (object, event, sender) = signed_message_object();
        let payload = event_payload(&event).unwrap();
        let wrong_sender = [9; 32];
        assert!(
            accept_inbound_session_object(
                &object,
                SessionObjectKind::Message,
                &payload,
                &wrong_sender,
                "session-object",
                32,
                300,
                &[],
            )
            .is_err()
        );
        assert!(
            accept_inbound_session_object(
                &object,
                SessionObjectKind::BotSettings,
                &payload,
                &sender,
                "session-object",
                32,
                300,
                &[],
            )
            .is_err()
        );
    }
}
