//! Pure workspace wire values shared by Node and native clients.
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
type DeviceId = String;
type EventHash = String;
type ScopeId = String;
type VersionVector = BTreeMap<DeviceId, u64>;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum InputTarget {
    BotMain {
        bot_id: String,
    },
    Task {
        task_id: String,
    },
    Work {
        work_id: String,
    },
    Routine {
        bot_id: String,
        trigger: String,
    },
    ChannelMember {
        channel_id: String,
        bot_id: String,
    },
    ChannelTask {
        channel_id: String,
        task_id: String,
        bot_id: String,
    },
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum InputIntent {
    Continue,
    NewTask,
    Defer,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct InputRoute {
    pub target: InputTarget,
    pub intent: InputIntent,
    pub sequence: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_revision: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct InputRequest {
    pub message_id: String,
    #[serde(flatten)]
    pub route: InputRoute,
    pub text: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attachments: Vec<Attachment>,
    pub report_results: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WorkspaceInput {
    #[serde(flatten)]
    pub input: InputRequest,
    pub conversation_id: ScopeId,
    pub source_event: EventHash,
    pub created_at_ms: i64,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TaskAction {
    Continue,
    Stop,
    Cancel,
    Defer,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AssignmentState {
    Proposed,
    Accepted,
    Adopted,
    Completed,
    Stopped,
    Unknown,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum InputControl {
    #[serde(deserialize_with = "empty_input_control")]
    Release,
    #[serde(deserialize_with = "empty_input_control")]
    Cancel,
    Edit {
        text: String,
        attachments: Vec<Attachment>,
    },
    Move {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        before_message_id: Option<String>,
    },
}

fn empty_input_control<'de, D: serde::Deserializer<'de>>(deserializer: D) -> Result<(), D::Error> {
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Empty {}
    Empty::deserialize(deserializer).map(|_| ())
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct QueuedInput {
    pub input: InputRequest,
    pub conversation_id: ScopeId,
    pub source_event: EventHash,
    pub control_event: EventHash,
    pub effective_order: u64,
    pub deferred: bool,
    pub cancelled: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum WorkspaceCommand {
    ControlInput {
        message_id: String,
        expected_event: EventHash,
        action: InputControl,
    },
    CreateChannel {
        channel_id: String,
        title: String,
        purpose: String,
        members: Vec<String>,
        coordinator_bot: String,
        coordinator_node: DeviceId,
    },
    ConfigureChannel {
        channel_id: String,
        expected_revision: u64,
        title: String,
        purpose: String,
        members: Vec<String>,
        coordinator_bot: String,
        coordinator_node: DeviceId,
        owner_epoch: u64,
    },
    PostChannelMessage {
        channel_id: String,
        message_id: String,
        text: String,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        attachments: Vec<Attachment>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        recipient_bot: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        task_id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reply_to: Option<String>,
    },
    CreateTask {
        task_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        channel_id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        parent_task_id: Option<String>,
        owner_bot: String,
        title: String,
        request: String,
        expected_result: String,
        source_events: Vec<EventHash>,
        dependencies: Vec<String>,
        reply_only: bool,
    },
    ReviseTask {
        task_id: String,
        expected_revision: u64,
        request: String,
        expected_result: String,
        dependencies: Vec<String>,
    },
    ControlTask {
        task_id: String,
        expected_revision: u64,
        action: TaskAction,
    },
    AssignTask {
        task_id: String,
        expected_revision: u64,
        assignment_id: String,
        bot_id: String,
        execution_node: DeviceId,
        bot_owner_epoch: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        channel_owner_epoch: Option<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        work_id: Option<String>,
        resource_roots: Vec<String>,
        automatic: bool,
    },
    AssignmentProgress {
        assignment_id: String,
        revision: u64,
        state: AssignmentState,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        execution: Option<TurnExecution>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        input_event: Option<EventHash>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        evidence_event: Option<EventHash>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        result: Option<String>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Channel {
    pub channel_id: String,
    pub title: String,
    pub purpose: String,
    pub members: Vec<String>,
    pub coordinator_bot: String,
    pub coordinator_node: DeviceId,
    pub owner_epoch: u64,
    pub revision: u64,
    pub event_hash: EventHash,
    pub created_at_ms: i64,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TaskState {
    NotStarted,
    Running,
    WaitingDependencies,
    WaitingUser,
    UpdatePending,
    StopPending,
    NeedsReconciliation,
    Completed,
    Cancelled,
    Deferred,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WorkspaceTask {
    pub task_id: String,
    pub channel_id: Option<String>,
    pub root_task_id: String,
    pub parent_task_id: Option<String>,
    pub owner_bot: String,
    pub title: String,
    pub request: String,
    pub expected_result: String,
    pub source_events: Vec<EventHash>,
    pub dependencies: Vec<String>,
    pub reply_only: bool,
    pub revision: u64,
    pub action: TaskAction,
    pub request_event: EventHash,
    pub event_hash: EventHash,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
    pub state: TaskState,
    pub waiting_reason: Option<String>,
    pub current_assignment: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AssignmentSubmission {
    pub revision: u64,
    pub text: String,
    pub event_hash: EventHash,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Assignment {
    pub submitted_result: Option<AssignmentSubmission>,
    pub trigger_message_id: Option<String>,
    pub source_event: Option<EventHash>,
    pub assignment_id: String,
    pub task_id: String,
    pub started_revision: u64,
    pub bot_id: String,
    pub execution_node: DeviceId,
    pub bot_owner_epoch: u64,
    pub channel_owner_epoch: Option<u64>,
    pub work_id: Option<String>,
    pub basis: VersionVector,
    pub resource_roots: Vec<String>,
    pub automatic: bool,
    pub accepted_revision: Option<u64>,
    pub adopted_revision: Option<u64>,
    pub completed_revision: Option<u64>,
    pub state: AssignmentState,
    pub result: Option<String>,
    pub event_hash: EventHash,
    pub created_at_ms: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum WorkspaceSpeaker {
    Human { device_id: DeviceId },
    Bot { bot_id: String },
    Node { device_id: DeviceId },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkspaceCoverage {
    pub limit: usize,
    pub truncated: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkspaceState {
    pub inputs: Vec<QueuedInput>,
    pub channels: Vec<Channel>,
    pub tasks: Vec<WorkspaceTask>,
    pub assignments: Vec<Assignment>,
    pub coverage: WorkspaceCoverage,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkspaceCommandReceipt {
    pub inputs: Vec<QueuedInput>,
    pub command_id: String,
    pub event_hash: EventHash,
    pub duplicate: bool,
    pub channels: Vec<Channel>,
    pub tasks: Vec<WorkspaceTask>,
    pub assignments: Vec<Assignment>,
}

impl WorkspaceCommand {
    pub fn channel_id(&self) -> Option<&str> {
        match self {
            Self::CreateChannel { channel_id, .. }
            | Self::ConfigureChannel { channel_id, .. }
            | Self::PostChannelMessage { channel_id, .. } => Some(channel_id),
            Self::CreateTask { channel_id, .. } => channel_id.as_deref(),
            _ => None,
        }
    }
    pub fn task_id(&self) -> Option<&str> {
        match self {
            Self::CreateTask { task_id, .. }
            | Self::ReviseTask { task_id, .. }
            | Self::ControlTask { task_id, .. }
            | Self::AssignTask { task_id, .. } => Some(task_id),
            Self::PostChannelMessage { task_id, .. } => task_id.as_deref(),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TurnExecution {
    NotStarted,
    Started,
    Unknown,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(deny_unknown_fields)]
pub struct BlobRef {
    pub hash: String,
    pub size: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(deny_unknown_fields)]
pub struct Attachment {
    pub hash: String,
    pub size: u64,
    pub name: String,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ScheduleAction {
    Pause,
    Resume,
    Delete,
}
impl Attachment {
    pub fn blob(&self) -> BlobRef {
        BlobRef {
            hash: self.hash.clone(),
            size: self.size,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn input_requires_an_explicit_boolean_reporting_choice() {
        let mut input = serde_json::json!({"message_id":"input-1","target":{"kind":"bot_main","bot_id":"bot-1"},"intent":"defer","sequence":1,"text":"Keep this input deferred"});
        assert!(
            serde_json::from_value::<InputRequest>(input.clone()).is_err(),
            "missing reporting choice must not become a default"
        );
        for invalid in [
            serde_json::Value::Null,
            serde_json::json!("true"),
            serde_json::json!(0),
        ] {
            input["report_results"] = invalid;
            assert!(
                serde_json::from_value::<InputRequest>(input.clone()).is_err(),
                "reporting choice must be a JSON boolean"
            );
        }
        input["report_results"] = serde_json::json!(true);
        let reports: InputRequest = serde_json::from_value(input.clone()).unwrap();
        assert!(reports.report_results);
        assert_eq!(
            serde_json::to_value(reports).unwrap()["report_results"],
            serde_json::json!(true)
        );
        input["report_results"] = serde_json::json!(false);
        let quiet: InputRequest = serde_json::from_value(input).unwrap();
        assert!(!quiet.report_results);
        assert_eq!(
            serde_json::to_value(quiet).unwrap()["report_results"],
            serde_json::json!(false)
        );
    }

    #[test]
    fn strict_input_request_rejects_unknown_flattened_fields() {
        for extra in [
            serde_json::Value::Null,
            serde_json::json!(true),
            serde_json::json!({"nested":"unexpected"}),
        ] {
            let mut input = serde_json::json!({"message_id":"m","target":{"kind":"task","task_id":"t"},"intent":"defer","sequence":0,"text":"Later","report_results":false});
            input["unexpected_input_field"] = extra;
            assert!(
                serde_json::from_value::<InputRequest>(input.clone()).is_err(),
                "accepted unknown flattened input field: {input}"
            );
        }
    }

    #[test]
    fn strict_input_controls_reject_unknown_unit_fields() {
        for kind in ["release", "cancel"] {
            let valid = serde_json::json!({"kind":kind});
            let control: InputControl = serde_json::from_value(valid.clone()).unwrap();
            assert_eq!(serde_json::to_value(control).unwrap(), valid);
            for extra in [
                serde_json::Value::Null,
                serde_json::json!(true),
                serde_json::json!({"nested":"unexpected"}),
            ] {
                let invalid = serde_json::json!({"kind":kind,"unexpected_control_field":extra});
                assert!(
                    serde_json::from_value::<InputControl>(invalid.clone()).is_err(),
                    "accepted unknown unit control field: {invalid}"
                );
                let command = serde_json::json!({"kind":"control_input","message_id":"m","expected_event":"e","action":invalid});
                assert!(serde_json::from_value::<WorkspaceCommand>(command).is_err());
            }
        }
    }

    #[test]
    fn input_request_preserves_null_defaults_and_attachment_fields() {
        let minimal = serde_json::json!({"message_id":"m","target":{"kind":"task","task_id":"t"},"intent":"defer","sequence":0,"text":"Later","report_results":false});
        let with_null_and_empty = serde_json::json!({"message_id":"m","target":{"kind":"task","task_id":"t"},"intent":"defer","sequence":0,"expected_revision":null,"text":"Later","attachments":[],"report_results":false});
        for document in [minimal.clone(), with_null_and_empty] {
            let input: InputRequest = serde_json::from_value(document).unwrap();
            assert_eq!(input.route.expected_revision, None);
            assert!(input.attachments.is_empty());
            assert_eq!(serde_json::to_value(input).unwrap(), minimal);
        }
        let full = serde_json::json!({"message_id":"m","target":{"kind":"bot_main","bot_id":"b"},"intent":"continue","sequence":4,"expected_revision":3,"text":"Read this","attachments":[{"hash":"fixture","size":1,"name":"a.txt"}],"report_results":true});
        let input: InputRequest = serde_json::from_value(full.clone()).unwrap();
        assert_eq!(input.route.expected_revision, Some(3));
        assert_eq!(input.attachments[0].name, "a.txt");
        assert!(input.report_results);
        assert_eq!(serde_json::to_value(input).unwrap(), full);
        for invalid in [
            serde_json::json!({"message_id":"m","target":{"kind":"task","task_id":"t","extra":null},"intent":"defer","sequence":0,"text":"Later","report_results":false}),
            serde_json::json!({"message_id":"m","target":{"kind":"task","task_id":"t"},"intent":"defer","sequence":0,"text":"Later","attachments":[{"hash":"fixture","size":1,"name":"a.txt","extra":true}],"report_results":false}),
            serde_json::json!({"message_id":"m","target":{"kind":"task","task_id":"t"},"intent":"defer","sequence":0,"text":"Later","attachments":null,"report_results":false}),
        ] {
            assert!(serde_json::from_value::<InputRequest>(invalid).is_err());
        }
    }

    #[test]
    fn input_controls_preserve_move_defaults_and_require_edit_attachments() {
        for document in [
            serde_json::json!({"kind":"move"}),
            serde_json::json!({"kind":"move","before_message_id":null}),
        ] {
            let control: InputControl = serde_json::from_value(document).unwrap();
            assert_eq!(
                control,
                InputControl::Move {
                    before_message_id: None
                }
            );
            assert_eq!(
                serde_json::to_value(control).unwrap(),
                serde_json::json!({"kind":"move"})
            );
        }
        for document in [
            serde_json::json!({"kind":"move","before_message_id":"next"}),
            serde_json::json!({"kind":"edit","text":"Updated","attachments":[]}),
        ] {
            let control: InputControl = serde_json::from_value(document.clone()).unwrap();
            assert_eq!(serde_json::to_value(control).unwrap(), document);
        }
        for invalid in [
            serde_json::json!({"kind":"edit","text":"Updated"}),
            serde_json::json!({"kind":"edit","text":"Updated","attachments":null}),
            serde_json::json!({"kind":"edit","text":"Updated","attachments":[],"extra":true}),
            serde_json::json!({"kind":"move","before_message_id":null,"extra":null}),
        ] {
            assert!(serde_json::from_value::<InputControl>(invalid).is_err());
        }
    }

    #[test]
    fn input_routes_preserve_optional_revisions_and_reject_unknown_fields() {
        let normalized = serde_json::json!({"target":{"kind":"task","task_id":"t"},"intent":"defer","sequence":0});
        for document in [
            normalized.clone(),
            serde_json::json!({"target":{"kind":"task","task_id":"t"},"intent":"defer","sequence":0,"expected_revision":null}),
        ] {
            let route: InputRoute = serde_json::from_value(document).unwrap();
            assert_eq!(route.expected_revision, None);
            assert_eq!(serde_json::to_value(route).unwrap(), normalized);
        }
        let versioned = serde_json::json!({"target":{"kind":"task","task_id":"t"},"intent":"continue","sequence":4,"expected_revision":3});
        let route: InputRoute = serde_json::from_value(versioned.clone()).unwrap();
        assert_eq!(route.expected_revision, Some(3));
        assert_eq!(serde_json::to_value(route).unwrap(), versioned);
        for invalid in [
            serde_json::json!({"target":{"kind":"task","task_id":"t"},"intent":"defer","sequence":0,"extra":null}),
            serde_json::json!({"target":{"kind":"task","task_id":"t","extra":true},"intent":"defer","sequence":0}),
            serde_json::json!({"target":{"kind":"task","task_id":"t"},"intent":"defer","sequence":"0"}),
            serde_json::json!({"target":{"kind":"task","task_id":"t"},"intent":"defer","sequence":0,"expected_revision":-1}),
        ] {
            assert!(serde_json::from_value::<InputRoute>(invalid).is_err());
        }
    }

    #[test]
    fn workspace_input_preserves_metadata_through_both_flattened_types() {
        let minimal = serde_json::json!({"message_id":"m","target":{"kind":"task","task_id":"t"},"intent":"defer","sequence":0,"text":"Later","report_results":false,"conversation_id":"c","source_event":"event","created_at_ms":123});
        let full = serde_json::json!({"message_id":"m","target":{"kind":"bot_main","bot_id":"b"},"intent":"continue","sequence":4,"expected_revision":3,"text":"Read this","attachments":[{"hash":"fixture","size":1,"name":"a.txt"}],"report_results":true,"conversation_id":"c","source_event":"event","created_at_ms":123});
        for document in [minimal.clone(), full] {
            let input: WorkspaceInput = serde_json::from_value(document.clone()).unwrap();
            assert_eq!(input.conversation_id, "c");
            assert_eq!(input.source_event, "event");
            assert_eq!(input.created_at_ms, 123);
            assert_eq!(
                input.input.route.expected_revision.is_some(),
                document.get("expected_revision").is_some()
            );
            assert_eq!(
                input.input.attachments.len(),
                document
                    .get("attachments")
                    .map_or(0, |value| value.as_array().unwrap().len())
            );
            let encoded = serde_json::to_value(&input).unwrap();
            assert_eq!(encoded, document);
            assert_eq!(
                serde_json::from_value::<WorkspaceInput>(encoded).unwrap(),
                input
            );
        }
        let with_null_and_empty = serde_json::json!({"message_id":"m","target":{"kind":"task","task_id":"t"},"intent":"defer","sequence":0,"expected_revision":null,"text":"Later","attachments":[],"report_results":false,"conversation_id":"c","source_event":"event","created_at_ms":123});
        let input: WorkspaceInput = serde_json::from_value(with_null_and_empty).unwrap();
        assert_eq!(input.input.route.expected_revision, None);
        assert!(input.input.attachments.is_empty());
        assert_eq!(serde_json::to_value(input).unwrap(), minimal);
    }

    #[test]
    fn workspace_input_and_commands_reject_unknown_nested_fields() {
        for invalid in [
            serde_json::json!({"message_id":"m","target":{"kind":"task","task_id":"t"},"intent":"defer","sequence":0,"text":"Later","report_results":false,"conversation_id":"c","source_event":"event","created_at_ms":123,"extra":null}),
            serde_json::json!({"message_id":"m","target":{"kind":"task","task_id":"t","extra":true},"intent":"defer","sequence":0,"text":"Later","report_results":false,"conversation_id":"c","source_event":"event","created_at_ms":123}),
            serde_json::json!({"message_id":"m","target":{"kind":"task","task_id":"t"},"intent":"defer","sequence":0,"text":"Later","attachments":[{"hash":"fixture","size":1,"name":"a.txt","extra":null}],"report_results":false,"conversation_id":"c","source_event":"event","created_at_ms":123}),
        ] {
            assert!(serde_json::from_value::<WorkspaceInput>(invalid).is_err());
        }
        assert!(serde_json::from_value::<WorkspaceCommand>(serde_json::json!({"kind":"control_input","message_id":"m","expected_event":"e","action":{"kind":"release"},"extra":null})).is_err());
    }

    #[test]
    fn all_six_input_targets_and_cas_controls_preserve_the_wire_shape() {
        let targets = [
            serde_json::json!({"kind":"bot_main","bot_id":"b"}),
            serde_json::json!({"kind":"task","task_id":"t"}),
            serde_json::json!({"kind":"work","work_id":"w"}),
            serde_json::json!({"kind":"routine","bot_id":"b","trigger":"r"}),
            serde_json::json!({"kind":"channel_member","channel_id":"c","bot_id":"b"}),
            serde_json::json!({"kind":"channel_task","channel_id":"c","task_id":"t","bot_id":"b"}),
        ];
        for value in targets {
            let target: InputTarget = serde_json::from_value(value.clone()).unwrap();
            assert_eq!(serde_json::to_value(target).unwrap(), value);
        }
        let value = serde_json::json!({"kind":"control_input","message_id":"m","expected_event":"original","action":{"kind":"move","before_message_id":"next"}});
        let command: WorkspaceCommand = serde_json::from_value(value.clone()).unwrap();
        assert_eq!(serde_json::to_value(command).unwrap(), value);
        assert!(
            serde_json::from_value::<InputTarget>(
                serde_json::json!({"kind":"task","task_id":"t","extra":1})
            )
            .is_err()
        );
    }
}
