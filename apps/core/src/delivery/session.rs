//! Safe event payloads for the encrypted session relay.
//!
//! This module only selects and serializes non-secret event data. The returned
//! bytes are plaintext input for a caller-owned encryption step.

use crate::model::EventBody;
use anyhow::{Result, bail};
use vibeface_delivery::{MAX_SESSION_OBJECT_BYTES, SessionObjectKind};

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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{EventBody, Role};

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
}
