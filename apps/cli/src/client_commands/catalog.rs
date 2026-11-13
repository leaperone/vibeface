use super::*;

// One declaration drives Clap, examples, metadata, and an exhaustive native match.
macro_rules! operation_catalog {
    ($command:ident, $native:ident, $kind:literal; $(
        $variant:ident, $pattern:pat, $name:literal, $tag:literal,
        $transport:literal, [$($method:literal),+], $about:literal, $example:literal;
    )+) => {
        #[derive(Subcommand)]
        pub enum $command {
            $(#[command(name = $name, about = $about, after_long_help = concat!(
                "Input example:\n  vibeface client ", $kind, " ", $name,
                " --input '", $example, "'\n\n",
                "Input accepts a JSON object, @FILE, or - for stdin, at most 1 MiB. ",
                "Omit the outer type tag. Unknown fields are rejected. ",
                "Native SDK staging validates domain preconditions before sending."
            ))]
            $variant(JsonInput),)+
        }
        impl $command {
            pub(super) fn into_request(self) -> Result<$native> {
                let request: $native = match self {
                    $(Self::$variant(input) => input.parse($tag)?,)+
                };
                // Adding a native variant requires a named command and catalog entry.
                match &request { $($pattern => {},)+ }
                check_nested(&request);
                Ok(request)
            }
            fn metadata() -> Vec<Value> {
                vec![$(json!({
                    "kind": $kind,
                    "command": concat!("client ", $kind, " ", $name),
                    "native_variant": stringify!($variant),
                    "tag": $tag,
                    "transport": $transport,
                    "core_methods": [$($method),+],
                    "example": serde_json::from_str::<Value>($example)
                        .expect("catalog example is valid JSON")
                })),+]
            }
        }
    }
}

operation_catalog!(ReadCommand, ReadRequest, "read";
    ConversationHistory, ReadRequest::ConversationHistory(_), "conversation-history", "conversation_history", "rpc", ["conversation_history"], "Read a bounded conversation history section; follow next_cursor until complete.", "{\"conversation_id\":\"conversation-1\",\"section\":\"messages\",\"limit\":50}";
    WriteState, ReadRequest::WriteState, "write-state", "write_state", "request", ["write_state"], "Read the Node write gate and recovery state.", "{}";
    Home, ReadRequest::Home, "home", "home", "rpc", ["bots"], "List Bots and the primary Bot.", "{}";
    Conversation, ReadRequest::Conversation{ .. }, "conversation", "conversation", "rpc", ["conversation"], "Read the initial conversation projection; use conversation-history for full paging.", "{\"conversation_id\":\"conversation-1\"}";
    Workspace, ReadRequest::Workspace, "workspace", "workspace", "rpc", ["workspace_state"], "Read full workspace state, including tasks, channels and deferred inputs.", "{}";
    WorkspaceOverview, ReadRequest::WorkspaceOverview, "workspace-overview", "workspace_overview", "rpc", ["workspace_overview"], "Read the workspace overview.", "{}";
    WorkspaceHistory, ReadRequest::WorkspaceHistory{ .. }, "workspace-history", "workspace_history", "rpc", ["workspace_history"], "Read workspace events, optionally filtered by channel or task.", "{\"channel_id\":null,\"task_id\":null,\"limit\":50}";
    Nodes, ReadRequest::Nodes, "nodes", "nodes", "rpc", ["nodes"], "List network Nodes.", "{}";
    NodeSettings, ReadRequest::NodeSettings, "node-settings", "node_settings", "request", ["node_settings"], "Read Node roots and manager settings.", "{}";
    Logs, ReadRequest::Logs, "logs", "logs", "request", ["logs"], "Read Node logs with default filters.", "{}";
    LogsFiltered, ReadRequest::LogsFiltered(_), "logs-filtered", "logs_filtered", "request", ["logs"], "Read logs by time, level, source, conversation, work or text.", "{\"since_ms\":0,\"until_ms\":null,\"level\":\"warn\",\"sources\":[\"node\"],\"conversation_id\":null,\"work_id\":null,\"text\":null,\"limit\":50}";
    Diagnostics, ReadRequest::Diagnostics, "diagnostics", "diagnostics", "request", ["diagnostics"], "Read Node diagnostics without a time filter.", "{}";
    DiagnosticsFiltered, ReadRequest::DiagnosticsFiltered{ .. }, "diagnostics-filtered", "diagnostics_filtered", "request", ["diagnostics"], "Read diagnostics within an explicit time window.", "{\"since_ms\":0,\"until_ms\":null}";
    Questions, ReadRequest::Questions, "questions", "questions", "rpc", ["questions"], "List unanswered Bot questions.", "{}";
    Notifications, ReadRequest::Notifications, "notifications", "notifications", "rpc", ["notifications"], "List notifications.", "{}";
    RelaySettings, ReadRequest::RelaySettings, "relay-settings", "relay_settings", "request", ["relay_settings"], "Read relay configuration.", "{}";
    TriageSettings, ReadRequest::TriageSettings, "triage-settings", "triage_settings", "request", ["triage_settings"], "Read decision forwarding settings.", "{}";
    ProviderModels, ReadRequest::ProviderModels{ .. }, "provider-models", "provider_models", "request", ["provider_models"], "Read the model catalog for a provider.", "{\"provider\":\"codex\"}";
    ProviderProbe, ReadRequest::ProviderProbe{ .. }, "provider-probe", "provider_probe", "request", ["provider_probe"], "Probe provider availability without running a model task.", "{\"provider\":\"codex\"}";
    TestCodexAppServer, ReadRequest::TestCodexAppServer, "test-codex-app-server", "test_codex_app_server", "request", ["test_codex_app_server"], "Test the configured Codex app-server connection.", "{}";
    Memory, ReadRequest::Memory{ .. }, "memory", "memory", "rpc", ["memory_state"], "Read a memory and its current version for a subsequent version-aware edit.", "{\"conversation_id\":\"conversation-1\",\"memory_id\":\"memory-1\"}";
    Work, ReadRequest::Work{ .. }, "work", "work", "rpc", ["work_detail"], "Read details for one work item.", "{\"conversation_id\":\"conversation-1\",\"work_id\":\"work-1\"}";
    Context, ReadRequest::Context{ .. }, "context", "context", "rpc", ["scope_context"], "Read context for a typed input target.", "{\"conversation_id\":\"conversation-1\",\"target\":{\"kind\":\"bot_main\",\"bot_id\":\"bot-1\"}}";
    Artifact, ReadRequest::Artifact{ .. }, "artifact", "artifact", "rpc", ["read_artifact"], "Read one text artifact page using byte offset and limit.", "{\"conversation_id\":\"conversation-1\",\"blob_hash\":\"0000000000000000000000000000000000000000000000000000000000000000\",\"offset\":0,\"limit\":65536}";
    ArtifactBytes, ReadRequest::ArtifactBytes{ .. }, "artifact-bytes", "artifact_bytes", "rpc", ["read_artifact"], "Read one binary artifact page. Use client files get for a verified complete download.", "{\"conversation_id\":\"conversation-1\",\"blob_hash\":\"0000000000000000000000000000000000000000000000000000000000000000\",\"offset\":0}";
    Delivery, ReadRequest::Delivery(_), "delivery", "delivery", "request", ["delivery"], "Read delivery with op status, list, providers or budgets and no other fields; get/read require order_id; find_intent requires intent_id.", "{\"op\":\"status\"}";
    OpenWorks, ReadRequest::OpenWorks, "open-works", "open_works", "rpc", ["open_works"], "List open work across conversations.", "{}";
    ShareLink, ReadRequest::ShareLink, "share-link", "share_link", "request", ["share_link"], "Read the selected Node share link.", "{}";
);

operation_catalog!(WriteCommand, Action, "write";
    Input, Action::Input{ .. }, "input", "input", "rpc", ["message"], "Submit input with a stable message ID, typed target and explicit reporting choice. Sequence 0 asks the SDK to allocate it durably.", "{\"conversation_id\":\"conversation-1\",\"input\":{\"message_id\":\"message-1\",\"target\":{\"kind\":\"bot_main\",\"bot_id\":\"bot-1\"},\"intent\":\"continue\",\"sequence\":0,\"text\":\"Hello\",\"report_results\":true}}";
    Workspace, Action::Workspace{ .. }, "workspace", "workspace", "rpc", ["workspace_command"], "Submit a typed workspace command with required revision or event preconditions. command.kind selects control_input, create_channel, configure_channel, post_channel_message, create_task, revise_task, control_task, assign_task or assignment_progress. control_input.action.kind selects release, cancel, edit or move. Run client capabilities for every nested variant and payload example.", "{\"command\":{\"kind\":\"control_task\",\"task_id\":\"task-1\",\"expected_revision\":1,\"action\":\"stop\"}}";
    Memory, Action::Memory{ .. }, "memory", "memory", "rpc", ["control_memory"], "Create, edit or delete memory using the exact base version. Null after deletes the memory.", "{\"conversation_id\":\"conversation-1\",\"memory_id\":\"memory-1\",\"base\":{\"epoch\":1,\"version\":null},\"after\":\"Remember this\"}";
    Schedule, Action::Schedule{ .. }, "schedule", "schedule", "rpc", ["control_schedule"], "Pause, resume or delete an existing schedule.", "{\"conversation_id\":\"conversation-1\",\"schedule_id\":\"schedule-1\",\"action\":\"pause\"}";
    StopReply, Action::StopReply{ .. }, "stop-reply", "stop_reply", "rpc", ["cancel_manager"], "Stop the reply identified by its original trigger.", "{\"conversation_id\":\"conversation-1\",\"trigger\":\"message-1\"}";
    StopWork, Action::StopWork{ .. }, "stop-work", "stop_work", "rpc", ["interrupt_work"], "Request that one work item stop.", "{\"conversation_id\":\"conversation-1\",\"work_id\":\"work-1\"}";
    Answer, Action::Answer{ .. }, "answer", "answer", "rpc", ["answer"], "Answer a Bot question in the signed conversation.", "{\"conversation_id\":\"conversation-1\",\"question_id\":\"question-1\",\"text\":\"Proceed\"}";
    CreateBot, Action::CreateBot{ .. }, "create-bot", "create_bot", "rpc", ["create_bot"], "Create a Bot with explicit stable IDs and scheduler ownership.", "{\"bot_id\":\"bot-1\",\"conversation_id\":\"conversation-1\",\"name\":\"Assistant\",\"instructions\":\"Help with this project\",\"scheduler_node\":\"node-1\",\"project\":null}";
    DeleteBot, Action::DeleteBot{ .. }, "delete-bot", "delete_bot", "rpc", ["delete_bot"], "Delete the selected Bot conversation.", "{\"conversation_id\":\"conversation-1\"}";
    SetPrimaryBot, Action::SetPrimaryBot{ .. }, "set-primary-bot", "set_primary_bot", "rpc", ["set_primary_bot"], "Choose the primary Bot conversation.", "{\"conversation_id\":\"conversation-1\"}";
    SetBotProject, Action::SetBotProject{ .. }, "set-bot-project", "set_bot_project", "rpc", ["set_bot_project"], "Set a Bot project, or clear it with project null.", "{\"conversation_id\":\"conversation-1\",\"project\":{\"target_node\":\"node-1\",\"cwd\":\"/workspace\"}}";
    SetBotAvatar, Action::SetBotAvatar{ .. }, "set-bot-avatar", "set_bot_avatar", "rpc", ["set_bot_avatar"], "Set or clear a Bot avatar using its expected version.", "{\"conversation_id\":\"conversation-1\",\"version\":null,\"avatar\":{\"outline\":\"round\",\"eyes\":\"dot\",\"hue\":180}}";
    NodeSettings, Action::NodeSettings(_), "node-settings", "node_settings", "request", ["configure_node"], "Configure Node roots and manager policy, including typed backup candidates.", "{\"roots\":[\"/workspace\"],\"manager\":null}";
    ProviderSettings, Action::ProviderSettings(_), "provider-settings", "provider_settings", "request", ["configure_provider"], "Configure provider roles, executable, environment and endpoint. Omitted endpoint keeps it; null removes it. Supply secrets through @FILE or stdin.", "{\"provider\":\"codex\",\"connect_local\":false,\"env\":{}}";
    Triage, Action::Triage(_), "triage", "triage", "request", ["configure_triage"], "Configure decision forwarding. Omitted webhook_secret keeps it; null clears it; a string replaces it. Supply secrets through @FILE or stdin.", "{\"default_outlet\":\"record\",\"projects\":[],\"webhook_url\":null,\"scan_codex_sessions\":false,\"codex_sessions\":null}";
    Relay, Action::Relay{ .. }, "relay", "relay", "request", ["configure_relay"], "Configure relay participation, address and optional positive rate/client limits.", "{\"volunteer\":false,\"listen\":null,\"url\":null,\"rate\":null,\"max_clients\":null,\"max_clients_per_ip\":null}";
    CreateDecisionToken, Action::CreateDecisionToken{ .. }, "create-decision-token", "create_decision_token", "request", ["create_decision_token"], "Create a decision token. The explicit secret result must be saved privately.", "{\"label\":\"CLI integration\"}";
    RevokeDecisionToken, Action::RevokeDecisionToken{ .. }, "revoke-decision-token", "revoke_decision_token", "request", ["revoke_decision_token"], "Revoke a decision token by ID.", "{\"token_id\":\"token-1\"}";
    TestWebhook, Action::TestWebhook, "test-webhook", "test_webhook", "request", ["test_webhook"], "Send a test through the configured decision webhook.", "{}";
    ConfirmWritable, Action::ConfirmWritable, "confirm-writable", "confirm_writable", "request", ["confirm_writable"], "Confirm the Node write gate after reviewing its write state.", "{}";
    AckNotification, Action::AckNotification{ .. }, "ack-notification", "ack_notification", "rpc", ["ack"], "Acknowledge a notification in its signed conversation.", "{\"conversation_id\":\"conversation-1\",\"notification_id\":\"notification-1\"}";
    ResolveCheck, Action::ResolveCheck{ .. }, "resolve-check", "resolve_check", "rpc", ["resolve"], "Resolve a check with a note; work checks also require work_id and completed/interrupted outcome.", "{\"conversation_id\":\"conversation-1\",\"check_id\":\"work:work-1\",\"work_id\":\"work-1\",\"outcome\":\"completed\",\"note\":\"Verified completion\"}";
    Delivery, Action::Delivery(_), "delivery", "delivery", "request", ["delivery"], "Configure or mutate delivery with op configure, prepare, confirm, accept, reconcile, confirm_batch, accept_batch or flush_budget. confirm requires intent_id; accept/reconcile require order_id; batch operations require exactly two intent_ids/order_ids; flush_budget requires budget_id and batch_id. Run client capabilities for complete nested payload examples.", "{\"op\":\"configure\",\"enabled\":true,\"charity_enabled\":true,\"paid_enabled\":false}";
    SetOwner, Action::SetOwner{ .. }, "set-owner", "set_owner", "rpc", ["set_owner"], "Change Bot conversation scheduler ownership using the next owner epoch.", "{\"conversation_id\":\"conversation-1\",\"scheduler_node\":\"node-1\",\"epoch\":2}";
    DismissWork, Action::DismissWork{ .. }, "dismiss-work", "dismiss_work", "rpc", ["dismiss_work"], "Dismiss a failed work item from the inbox.", "{\"conversation_id\":\"conversation-1\",\"work_id\":\"work-1\"}";
    Unanswered, Action::Unanswered{ .. }, "unanswered", "unanswered", "rpc", ["reply_unanswered", "dismiss_unanswered"], "Reply to or dismiss selected unanswered messages. Select action reply or dismiss.", "{\"conversation_id\":\"conversation-1\",\"message_ids\":[\"message-1\"],\"action\":\"reply\"}";
    LocalAccess, Action::LocalAccess{ .. }, "local-access", "local_access", "request", ["local_access"], "Enable or disable local access under the current session authority.", "{\"enabled\":false}";
);

pub(super) fn operations() -> Vec<Value> {
    let mut operations = ReadCommand::metadata();
    operations.extend(WriteCommand::metadata());
    operations
}

trait NestedCoverage {
    fn check_nested(&self);
}
fn check_nested(request: &impl NestedCoverage) {
    request.check_nested();
}
impl NestedCoverage for ReadRequest {
    fn check_nested(&self) {
        match self {
            Self::Delivery(value) => {
                delivery_read_tag(value);
            }
            Self::Context { target, .. } => {
                input_target_tag(target);
            }
            _ => {}
        }
    }
}
impl NestedCoverage for Action {
    fn check_nested(&self) {
        match self {
            Self::Delivery(value) => {
                delivery_action_tag(value);
            }
            Self::Workspace { command } => {
                workspace_command_tag(command);
                if let vibeface_client::wire::WorkspaceCommand::ControlInput { action, .. } =
                    command
                {
                    input_control_tag(action);
                }
            }
            Self::Input { input, .. } => {
                input_target_tag(&input.route.target);
            }
            _ => {}
        }
    }
}

macro_rules! nested_catalog {
    ($function:ident, $metadata:ident, $native:ty, $discriminator:literal; $(
        $pattern:pat => $variant:literal, $tag:literal, $example:literal;
    )+) => {
        fn $function(value: &$native) -> &'static str {
            match value { $($pattern => $tag,)+ }
        }
        fn $metadata() -> Value {
            json!({"tag": $discriminator, "variants": [$(json!({
                "native_variant": $variant,
                "tag": $tag,
                "example": serde_json::from_str::<Value>($example)
                    .expect("nested catalog example is valid JSON")
            })),+]})
        }
    }
}

use vibeface_client::wire::{InputControl, InputTarget, WorkspaceCommand};
use vibeface_client::{DeliveryAction, DeliveryRead};

nested_catalog!(delivery_read_tag, delivery_read_metadata, DeliveryRead, "op";
    DeliveryRead::Status => "Status", "status", "{\"op\":\"status\"}";
    DeliveryRead::List => "List", "list", "{\"op\":\"list\"}";
    DeliveryRead::Providers => "Providers", "providers", "{\"op\":\"providers\"}";
    DeliveryRead::Budgets => "Budgets", "budgets", "{\"op\":\"budgets\"}";
    DeliveryRead::Get { .. } => "Get", "get", "{\"op\":\"get\",\"order_id\":\"order-1\"}";
    DeliveryRead::FindIntent { .. } => "FindIntent", "find_intent", "{\"op\":\"find_intent\",\"intent_id\":\"intent-1\"}";
    DeliveryRead::Read { .. } => "Read", "read", "{\"op\":\"read\",\"order_id\":\"order-1\"}";
);

nested_catalog!(delivery_action_tag, delivery_action_metadata, DeliveryAction, "op";
    DeliveryAction::Configure { .. } => "Configure", "configure", "{\"op\":\"configure\",\"enabled\":true,\"charity_enabled\":true,\"paid_enabled\":false}";
    DeliveryAction::Prepare(_) => "Prepare", "prepare", "{\"op\":\"prepare\",\"intent_id\":\"intent-1\",\"receiver_peer\":\"receiver\",\"provider_peer\":\"provider\",\"mode\":\"charity\",\"acceptance\":\"recoverable\",\"name\":\"hello.txt\",\"content_base64\":\"aGVsbG8=\",\"max_service_amount\":\"0\",\"max_gas_amount\":\"0\",\"budget_id\":null}";
    DeliveryAction::Confirm { .. } => "Confirm", "confirm", "{\"op\":\"confirm\",\"intent_id\":\"intent-1\"}";
    DeliveryAction::Accept { .. } => "Accept", "accept", "{\"op\":\"accept\",\"order_id\":\"order-1\"}";
    DeliveryAction::Reconcile { .. } => "Reconcile", "reconcile", "{\"op\":\"reconcile\",\"order_id\":\"order-1\"}";
    DeliveryAction::ConfirmBatch { .. } => "ConfirmBatch", "confirm_batch", "{\"op\":\"confirm_batch\",\"intent_ids\":[\"intent-1\",\"intent-2\"]}";
    DeliveryAction::AcceptBatch { .. } => "AcceptBatch", "accept_batch", "{\"op\":\"accept_batch\",\"order_ids\":[\"order-1\",\"order-2\"]}";
    DeliveryAction::FlushBudget { .. } => "FlushBudget", "flush_budget", "{\"op\":\"flush_budget\",\"budget_id\":\"0000000000000000000000000000000000000000000000000000000000000000\",\"batch_id\":\"1111111111111111111111111111111111111111111111111111111111111111\"}";
);

nested_catalog!(workspace_command_tag, workspace_command_metadata, WorkspaceCommand, "kind";
    WorkspaceCommand::ControlInput { .. } => "ControlInput", "control_input", "{\"kind\":\"control_input\",\"message_id\":\"message-1\",\"expected_event\":\"0000000000000000000000000000000000000000000000000000000000000000\",\"action\":{\"kind\":\"release\"}}";
    WorkspaceCommand::CreateChannel { .. } => "CreateChannel", "create_channel", "{\"kind\":\"create_channel\",\"channel_id\":\"channel-1\",\"title\":\"Project\",\"purpose\":\"Discuss project work\",\"members\":[\"bot-1\"],\"coordinator_bot\":\"bot-1\",\"coordinator_node\":\"node-1\"}";
    WorkspaceCommand::ConfigureChannel { .. } => "ConfigureChannel", "configure_channel", "{\"kind\":\"configure_channel\",\"channel_id\":\"channel-1\",\"expected_revision\":1,\"title\":\"Project\",\"purpose\":\"Discuss project work\",\"members\":[\"bot-1\"],\"coordinator_bot\":\"bot-1\",\"coordinator_node\":\"node-1\",\"owner_epoch\":1}";
    WorkspaceCommand::PostChannelMessage { .. } => "PostChannelMessage", "post_channel_message", "{\"kind\":\"post_channel_message\",\"channel_id\":\"channel-1\",\"message_id\":\"message-1\",\"text\":\"Hello\"}";
    WorkspaceCommand::CreateTask { .. } => "CreateTask", "create_task", "{\"kind\":\"create_task\",\"task_id\":\"task-1\",\"owner_bot\":\"bot-1\",\"title\":\"Review\",\"request\":\"Review this change\",\"expected_result\":\"Findings\",\"source_events\":[],\"dependencies\":[],\"reply_only\":false}";
    WorkspaceCommand::ReviseTask { .. } => "ReviseTask", "revise_task", "{\"kind\":\"revise_task\",\"task_id\":\"task-1\",\"expected_revision\":1,\"request\":\"Review updated change\",\"expected_result\":\"Findings\",\"dependencies\":[]}";
    WorkspaceCommand::ControlTask { .. } => "ControlTask", "control_task", "{\"kind\":\"control_task\",\"task_id\":\"task-1\",\"expected_revision\":1,\"action\":\"stop\"}";
    WorkspaceCommand::AssignTask { .. } => "AssignTask", "assign_task", "{\"kind\":\"assign_task\",\"task_id\":\"task-1\",\"expected_revision\":1,\"assignment_id\":\"assignment-1\",\"bot_id\":\"bot-1\",\"execution_node\":\"node-1\",\"bot_owner_epoch\":1,\"resource_roots\":[\"/workspace\"],\"automatic\":false}";
    WorkspaceCommand::AssignmentProgress { .. } => "AssignmentProgress", "assignment_progress", "{\"kind\":\"assignment_progress\",\"assignment_id\":\"assignment-1\",\"revision\":1,\"state\":\"accepted\"}";
);

nested_catalog!(input_target_tag, input_target_metadata, InputTarget, "kind";
    InputTarget::BotMain { .. } => "BotMain", "bot_main", "{\"kind\":\"bot_main\",\"bot_id\":\"bot-1\"}";
    InputTarget::Task { .. } => "Task", "task", "{\"kind\":\"task\",\"task_id\":\"task-1\"}";
    InputTarget::Work { .. } => "Work", "work", "{\"kind\":\"work\",\"work_id\":\"work-1\"}";
    InputTarget::Routine { .. } => "Routine", "routine", "{\"kind\":\"routine\",\"bot_id\":\"bot-1\",\"trigger\":\"trigger-1\"}";
    InputTarget::ChannelMember { .. } => "ChannelMember", "channel_member", "{\"kind\":\"channel_member\",\"channel_id\":\"channel-1\",\"bot_id\":\"bot-1\"}";
    InputTarget::ChannelTask { .. } => "ChannelTask", "channel_task", "{\"kind\":\"channel_task\",\"channel_id\":\"channel-1\",\"task_id\":\"task-1\",\"bot_id\":\"bot-1\"}";
);

nested_catalog!(input_control_tag, input_control_metadata, InputControl, "kind";
    InputControl::Release => "Release", "release", "{\"kind\":\"release\"}";
    InputControl::Cancel => "Cancel", "cancel", "{\"kind\":\"cancel\"}";
    InputControl::Edit { .. } => "Edit", "edit", "{\"kind\":\"edit\",\"text\":\"Updated input\",\"attachments\":[]}";
    InputControl::Move { .. } => "Move", "move", "{\"kind\":\"move\",\"before_message_id\":\"message-2\"}";
);

pub(super) fn nested_capabilities() -> Value {
    json!({
        "DeliveryRead": delivery_read_metadata(),
        "DeliveryAction": delivery_action_metadata(),
        "WorkspaceCommand": workspace_command_metadata(),
        "InputTarget": input_target_metadata(),
        "InputControl": input_control_metadata(),
        "InputIntent": ["continue", "new_task", "defer"],
        "TaskAction": ["continue", "stop", "cancel", "defer"],
        "ScheduleAction": ["pause", "resume", "delete"],
        "UnansweredAction": ["reply", "dismiss"],
        "ResolveOutcome": ["completed", "interrupted"],
        "AssignmentState": ["proposed", "accepted", "adopted", "completed", "stopped", "unknown"]
    })
}
