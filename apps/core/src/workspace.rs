//! Personal workspace commands and projections share the signed event log.
use crate::{
    model::{DeviceId, EventHash, ScopeId},
    scheduling::SessionScope,
};
use serde::{Deserialize, Serialize};
pub use vibeface_protocol::workspace::{
    Assignment, AssignmentState, AssignmentSubmission, Channel, InputControl, InputIntent,
    InputRequest, InputRoute, InputTarget, QueuedInput, TaskAction, TaskState, WorkspaceCommand,
    WorkspaceCommandReceipt, WorkspaceCoverage, WorkspaceInput, WorkspaceSpeaker, WorkspaceState,
    WorkspaceTask,
};

pub const WORKSPACE_SCOPE: &str = "workspace";
pub const MAX_AUTOMATIC_ASSIGNMENTS: usize = 8;
pub const MAX_TASK_DEPTH: usize = 4;
pub const MAX_MEMBER_REPLIES: usize = 2;

/// RPC dispatch, scope checks and attestation validation own these envelope fields.
pub(crate) fn parse_message_input(
    envelope: &serde_json::Value,
) -> Result<InputRequest, serde_json::Error> {
    let mut document = envelope.clone();
    if let Some(fields) = document.as_object_mut() {
        for name in ["method", "client_id", "conversation_id", "attestation"] {
            fields.remove(name);
        }
    }
    serde_json::from_value(document)
}

pub(crate) fn input_event_body(
    input: &InputRequest,
    attestation: Option<crate::model::Attestation>,
) -> crate::model::EventBody {
    let InputRequest {
        message_id,
        route:
            InputRoute {
                target,
                intent,
                sequence,
                expected_revision,
            },
        text,
        attachments,
        report_results,
    } = input;
    crate::model::EventBody::InputPosted {
        message_id: message_id.clone(),
        target: target.clone(),
        intent: *intent,
        sequence: *sequence,
        expected_revision: *expected_revision,
        text: text.clone(),
        attachments: attachments.clone(),
        report_results: Some(*report_results),
        attestation,
    }
}

/// A ControlInput qualification or CAS check rejected the command before any append.
#[derive(Debug)]
pub(crate) struct WorkspaceCommandNotSaved {
    pub command_id: String,
    pub reason: String,
}

impl WorkspaceCommandNotSaved {
    pub(crate) fn new(command_id: &str, reason: &'static str) -> Self {
        Self {
            command_id: command_id.to_owned(),
            reason: reason.to_owned(),
        }
    }
}

impl std::fmt::Display for WorkspaceCommandNotSaved {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.reason)
    }
}

impl std::error::Error for WorkspaceCommandNotSaved {}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ResolvedInputTarget {
    pub target: InputTarget,
    pub conversation_id: ScopeId,
    pub bot_id: String,
    pub owner_node: DeviceId,
    pub owner_epoch: u64,
    pub scope: SessionScope,
    pub task_id: Option<String>,
    pub task_revision: Option<u64>,
    pub assignment_id: Option<String>,
    pub channel_id: Option<String>,
    pub channel_owner_epoch: Option<u64>,
    pub reply_only: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct WorkspaceHistoryItem {
    pub speaker: WorkspaceSpeaker,
    pub event_hash: EventHash,
    pub created_at_ms: i64,
    pub scope_id: String,
    pub kind: String,
    pub body: crate::model::EventBody,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        crypto::{LocalIdentity, attestation_payload, request_key, verify_attestation},
        model::Attestation,
    };
    use anyhow::Result;
    use serde_json::{Value, json};

    #[test]
    fn message_input_accepts_only_known_rpc_metadata_without_changing_the_envelope() -> Result<()> {
        let envelope = json!({
            "method": "message", "client_id": "client", "conversation_id": "chat",
            "attestation": {"device_id": "client", "signature": "signature"},
            "message_id": "m", "target": {"kind": "bot_main", "bot_id": "bot"},
            "intent": "continue", "sequence": 1, "text": "hi", "report_results": true
        });
        let original = envelope.clone();
        let expected = InputRequest {
            message_id: "m".into(),
            route: InputRoute {
                target: InputTarget::BotMain {
                    bot_id: "bot".into(),
                },
                intent: InputIntent::Continue,
                sequence: 1,
                expected_revision: None,
            },
            text: "hi".into(),
            attachments: vec![],
            report_results: true,
        };
        assert_eq!(parse_message_input(&envelope)?, expected);
        assert_eq!(envelope, original);
        let mut receipt = envelope.clone();
        receipt.as_object_mut().unwrap().remove("method");
        receipt.as_object_mut().unwrap().remove("client_id");
        assert_eq!(parse_message_input(&receipt)?, expected);
        for extra in [Value::Null, json!(true), json!({"nested": "extra"})] {
            let mut invalid = envelope.clone();
            invalid["unexpected_input_field"] = extra.clone();
            assert!(parse_message_input(&invalid).is_err());
            let mut invalid = envelope.clone();
            invalid["target"]["unexpected_target_field"] = extra;
            assert!(parse_message_input(&invalid).is_err());
        }
        Ok(())
    }

    #[test]
    fn input_event_body_preserves_literal_attestation_payload() -> Result<()> {
        let network = "d75a980182b10ab7d54bfed3c964073a0ee172f3daa62325af021a68f707511a";
        let input: InputRequest = serde_json::from_value(json!({
            "message_id": "m1",
            "target": {"kind": "channel_task", "channel_id": "channel", "task_id": "task", "bot_id": "bot"},
            "intent": "new_task", "sequence": 17, "expected_revision": 7,
            "text": "hi\n雪e\u{301} ",
            "attachments": [
                {"hash": "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb", "size": 5, "name": "一.txt"},
                {"hash": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa", "size": 3, "name": "two.txt"}
            ],
            "report_results": false
        }))?;
        let expected: Value = serde_json::from_str(
            r#"{"type":"input_posted","message_id":"m1","target":{"kind":"channel_task","channel_id":"channel","task_id":"task","bot_id":"bot"},"intent":"new_task","sequence":17,"expected_revision":7,"text":"hi\n雪é ","attachments":[{"hash":"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb","size":5,"name":"一.txt"},{"hash":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","size":3,"name":"two.txt"}],"report_results":false}"#,
        )?;
        let payload = r#"{"body":{"attachments":[{"hash":"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb","name":"一.txt","size":5},{"hash":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","name":"two.txt","size":3}],"expected_revision":7,"intent":"new_task","message_id":"m1","report_results":false,"sequence":17,"target":{"bot_id":"bot","channel_id":"channel","kind":"channel_task","task_id":"task"},"text":"hi\n雪é ","type":"input_posted"},"network_id":"d75a980182b10ab7d54bfed3c964073a0ee172f3daa62325af021a68f707511a","scope_id":"chat"}"#;
        let unsigned = input_event_body(&input, None);
        assert_eq!(serde_json::to_value(&unsigned)?, expected);
        assert_eq!(
            attestation_payload(network, "chat", &unsigned)?,
            payload.as_bytes()
        );

        let client = LocalIdentity::generate();
        let attestation = Attestation {
            device_id: client.device_id()?,
            signature: client.sign_payload(
                vibeface_secure::join::ATTESTATION_DOMAIN,
                payload.as_bytes(),
            )?,
        };
        let signed = input_event_body(&input, Some(attestation.clone()));
        let mut expected_signed = expected;
        expected_signed["attestation"] = serde_json::to_value(&attestation)?;
        assert_eq!(serde_json::to_value(&signed)?, expected_signed);
        assert_eq!(signed.attestation(), Some(&attestation));
        assert_eq!(
            attestation_payload(network, "chat", &signed)?,
            payload.as_bytes()
        );
        verify_attestation(network, "chat", &signed)?;
        assert_eq!(
            request_key(network, "chat", &signed, "entry-one")?,
            request_key(network, "chat", &signed, "entry-two")?,
        );
        let independent: crate::model::EventBody = serde_json::from_value(expected_signed)?;
        verify_attestation(network, "chat", &independent)?;
        Ok(())
    }

    #[test]
    fn input_event_body_keeps_each_target_and_optional_wire_field() -> Result<()> {
        let cases = [
            (
                InputTarget::BotMain {
                    bot_id: "main".into(),
                },
                InputIntent::Continue,
                None,
                r#"{"type":"input_posted","message_id":"original","target":{"kind":"bot_main","bot_id":"main"},"intent":"continue","sequence":0,"text":" ","report_results":true}"#,
                r#"{"body":{"intent":"continue","message_id":"original","report_results":true,"sequence":0,"target":{"bot_id":"main","kind":"bot_main"},"text":" ","type":"input_posted"},"network_id":"d75a980182b10ab7d54bfed3c964073a0ee172f3daa62325af021a68f707511a","scope_id":"scope"}"#,
            ),
            (
                InputTarget::Task {
                    task_id: "task".into(),
                },
                InputIntent::Defer,
                Some(0),
                r#"{"type":"input_posted","message_id":"original","target":{"kind":"task","task_id":"task"},"intent":"defer","sequence":0,"expected_revision":0,"text":" ","report_results":true}"#,
                r#"{"body":{"expected_revision":0,"intent":"defer","message_id":"original","report_results":true,"sequence":0,"target":{"kind":"task","task_id":"task"},"text":" ","type":"input_posted"},"network_id":"d75a980182b10ab7d54bfed3c964073a0ee172f3daa62325af021a68f707511a","scope_id":"scope"}"#,
            ),
            (
                InputTarget::Work {
                    work_id: "work".into(),
                },
                InputIntent::NewTask,
                None,
                r#"{"type":"input_posted","message_id":"original","target":{"kind":"work","work_id":"work"},"intent":"new_task","sequence":0,"text":" ","report_results":true}"#,
                r#"{"body":{"intent":"new_task","message_id":"original","report_results":true,"sequence":0,"target":{"kind":"work","work_id":"work"},"text":" ","type":"input_posted"},"network_id":"d75a980182b10ab7d54bfed3c964073a0ee172f3daa62325af021a68f707511a","scope_id":"scope"}"#,
            ),
            (
                InputTarget::Routine {
                    bot_id: "routine".into(),
                    trigger: "daily:7".into(),
                },
                InputIntent::Continue,
                None,
                r#"{"type":"input_posted","message_id":"original","target":{"kind":"routine","bot_id":"routine","trigger":"daily:7"},"intent":"continue","sequence":0,"text":" ","report_results":true}"#,
                r#"{"body":{"intent":"continue","message_id":"original","report_results":true,"sequence":0,"target":{"bot_id":"routine","kind":"routine","trigger":"daily:7"},"text":" ","type":"input_posted"},"network_id":"d75a980182b10ab7d54bfed3c964073a0ee172f3daa62325af021a68f707511a","scope_id":"scope"}"#,
            ),
            (
                InputTarget::ChannelMember {
                    channel_id: "channel".into(),
                    bot_id: "member".into(),
                },
                InputIntent::Defer,
                Some(0),
                r#"{"type":"input_posted","message_id":"original","target":{"kind":"channel_member","channel_id":"channel","bot_id":"member"},"intent":"defer","sequence":0,"expected_revision":0,"text":" ","report_results":true}"#,
                r#"{"body":{"expected_revision":0,"intent":"defer","message_id":"original","report_results":true,"sequence":0,"target":{"bot_id":"member","channel_id":"channel","kind":"channel_member"},"text":" ","type":"input_posted"},"network_id":"d75a980182b10ab7d54bfed3c964073a0ee172f3daa62325af021a68f707511a","scope_id":"scope"}"#,
            ),
        ];
        let client = LocalIdentity::generate();
        for (target, intent, expected_revision, expected, payload) in cases {
            let input = InputRequest {
                message_id: "original".into(),
                route: InputRoute {
                    target,
                    intent,
                    sequence: 0,
                    expected_revision,
                },
                text: " ".into(),
                attachments: vec![],
                report_results: true,
            };
            let body = input_event_body(&input, None);
            assert_eq!(
                serde_json::to_value(&body)?,
                serde_json::from_str::<Value>(expected)?
            );
            assert_eq!(
                attestation_payload(
                    "d75a980182b10ab7d54bfed3c964073a0ee172f3daa62325af021a68f707511a",
                    "scope",
                    &body
                )?,
                payload.as_bytes()
            );
            let attestation = Attestation {
                device_id: client.device_id()?,
                signature: client.sign_payload(
                    vibeface_secure::join::ATTESTATION_DOMAIN,
                    payload.as_bytes(),
                )?,
            };
            let signed = input_event_body(&input, Some(attestation.clone()));
            let mut expected_signed: Value = serde_json::from_str(expected)?;
            expected_signed["attestation"] = serde_json::to_value(&attestation)?;
            assert_eq!(serde_json::to_value(&signed)?, expected_signed);
            verify_attestation(
                "d75a980182b10ab7d54bfed3c964073a0ee172f3daa62325af021a68f707511a",
                "scope",
                &signed,
            )?;
        }
        Ok(())
    }
}
