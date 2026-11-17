//! Core command identities and static policy. Payload validation stays in its handler.
use anyhow::{Result, ensure};
use serde_json::Value;

#[derive(Debug, Clone)]
pub(crate) struct Params(Value);
impl Params {
    pub(crate) fn new(value: Value) -> Result<Self> {
        ensure!(value.is_object(), "invalid client method or params");
        Ok(Self(value))
    }
    pub(crate) fn into_value(self) -> Value {
        self.0
    }
}

#[derive(Debug, Clone)]
pub(crate) struct BusinessParams(Value);
impl BusinessParams {
    pub(crate) fn empty() -> Self {
        Self(serde_json::json!({}))
    }
    pub(crate) fn value(&self) -> &Value {
        &self.0
    }
    pub(crate) fn bind_client(&mut self, subject: &str) {
        self.0["client_id"] = Value::String(subject.to_owned());
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Lane {
    Long,
    Read,
    Control,
    Ordered,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ReadConsistency {
    CommittedDisplay,
    AuthoritativeWriter,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MaintenancePolicy {
    Exempt,
    Enter,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum OperatorPolicy {
    Allowed,
    Signed,
    Unavailable,
    JoinStatus,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DeliveryCheck {
    Captured,
    InfoNetwork,
    ReturnedPrincipal,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SessionEffect {
    None,
    Subscribe,
    Observe,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct ClientPolicy {
    pub(crate) lane: Lane,
    pub(crate) maintenance: MaintenancePolicy,
    pub(crate) guest: bool,
    pub(crate) transition: bool,
    pub(crate) operator: OperatorPolicy,
    pub(crate) delivery: DeliveryCheck,
    pub(crate) effect: SessionEffect,
    pub(crate) consistency: ReadConsistency,
}
const REJECTED_CLIENT: ClientPolicy = ClientPolicy {
    lane: Lane::Ordered,
    maintenance: MaintenancePolicy::Enter,
    guest: false,
    transition: false,
    operator: OperatorPolicy::Allowed,
    delivery: DeliveryCheck::Captured,
    effect: SessionEffect::None,
    consistency: ReadConsistency::AuthoritativeWriter,
};
#[derive(Debug, Clone)]
pub(crate) enum CommandRejection {
    OuterName,
}
pub(crate) type ClientInput = std::result::Result<ClientCommand, CommandRejection>;
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BusinessMethodError {
    Required,
    Invalid,
}
#[derive(Debug, Clone)]
pub(crate) enum BusinessRejection {
    MethodField {
        error: BusinessMethodError,
        params: BusinessParams,
    },
    UnsupportedName {
        params: BusinessParams,
    },
}
pub(crate) type BusinessInput = std::result::Result<BusinessCommand, BusinessRejection>;

macro_rules! client_commands {
    ($( $variant:ident($wire:literal, $payload:ty, $assemble:expr) =>
        ($lane:ident, $maintenance:ident, $guest:literal, $transition:literal,
         $operator:ident, $delivery:ident, $effect:ident); )*) => {
        #[derive(Debug, Clone, Copy, PartialEq, Eq)]
        pub(crate) enum ClientName { $( $variant, )* }
        #[derive(Debug, Clone)]
        pub(crate) enum ClientCommand { $( $variant($payload), )* }
        impl ClientName {
            pub(crate) fn decode(name: &str) -> std::result::Result<Self, CommandRejection> {
                match name { $( $wire => Ok(Self::$variant), )* _ => Err(CommandRejection::OuterName) }
            }
            pub(crate) fn wire_name(self) -> &'static str {
                match self { $( Self::$variant => $wire, )* }
            }
            pub(crate) fn policy(self) -> ClientPolicy {
                match self { $( Self::$variant => ClientPolicy {
                    lane: Lane::$lane, maintenance: MaintenancePolicy::$maintenance,
                    guest: $guest, transition: $transition, operator: OperatorPolicy::$operator,
                    delivery: DeliveryCheck::$delivery, effect: SessionEffect::$effect,
                    consistency: ReadConsistency::AuthoritativeWriter,
                }, )* }
            }
            pub(crate) fn assemble(self, params: Params) -> ClientCommand {
                match self { $( Self::$variant => ClientCommand::$variant(($assemble)(params)), )* }
            }
        }
        impl ClientCommand {
            fn name(&self) -> ClientName {
                match self { $( Self::$variant(_) => ClientName::$variant, )* }
            }
            pub(crate) fn policy(&self) -> ClientPolicy {
                let mut policy = self.name().policy();
                if let Self::Rpc(Ok(command)) = self {
                    policy.lane = command.lane();
                    policy.consistency = command.consistency();
                    policy.effect = command.effect();
                }
                policy
            }
        }
    }
}

client_commands! {
    Info("info", Params, |params| params) => (Read, Exempt, true, false, Allowed, InfoNetwork, None);
    Authenticate("authenticate", Params, |params| params) => (Ordered, Exempt, true, false, Allowed, ReturnedPrincipal, None);
    Directory("directory", Params, |params| params) => (Read, Exempt, false, false, Allowed, Captured, None);
    WriteState("write_state", Params, |params| params) => (Read, Enter, false, false, Allowed, Captured, None);
    Diagnostics("diagnostics", Params, |params| params) => (Long, Enter, false, false, Allowed, Captured, None);
    Log("log", Params, |params| params) => (Ordered, Enter, false, false, Unavailable, Captured, None);
    Logs("logs", Params, |params| params) => (Long, Enter, false, false, Allowed, Captured, None);
    ConfirmWritable("confirm_writable", Params, |params| params) => (Ordered, Enter, false, false, Allowed, Captured, None);
    NodeSettings("node_settings", Params, |params| params) => (Read, Exempt, false, false, Allowed, Captured, None);
    Delivery("delivery", Params, |params| params) => (Ordered, Enter, false, false, Allowed, Captured, None);
    ConfigureNode("configure_node", Params, |params| params) => (Ordered, Enter, false, false, Allowed, Captured, None);
    ConfigureProvider("configure_provider", Params, |params| params) => (Ordered, Enter, false, false, Allowed, Captured, None);
    TestCodexAppServer("test_codex_app_server", Params, |params| params) => (Long, Enter, false, false, Allowed, Captured, None);
    ProviderModels("provider_models", Params, |params| params) => (Long, Exempt, false, false, Allowed, Captured, None);
    ProviderProbe("provider_probe", Params, |params| params) => (Long, Exempt, false, false, Allowed, Captured, None);
    TriageSettings("triage_settings", Params, |params| params) => (Read, Enter, false, false, Allowed, Captured, None);
    ConfigureTriage("configure_triage", Params, |params| params) => (Ordered, Enter, false, false, Allowed, Captured, None);
    RelaySettings("relay_settings", Params, |params| params) => (Read, Enter, false, false, Allowed, Captured, None);
    ShareLink("share_link", Params, |params| params) => (Read, Enter, false, false, Allowed, Captured, None);
    ConfigureRelay("configure_relay", Params, |params| params) => (Ordered, Enter, false, false, Allowed, Captured, None);
    TestWebhook("test_webhook", Params, |params| params) => (Long, Enter, false, false, Allowed, Captured, None);
    CreateDecisionToken("create_decision_token", Params, |params| params) => (Ordered, Enter, false, false, Allowed, Captured, None);
    RevokeDecisionToken("revoke_decision_token", Params, |params| params) => (Ordered, Enter, false, false, Allowed, Captured, None);
    Subscribe("subscribe", Params, |params| params) => (Ordered, Exempt, false, false, Unavailable, Captured, Subscribe);
    Rpc("rpc", BusinessInput, decode_business) => (Ordered, Enter, false, false, Allowed, Captured, None);
    LocalAccess("local_access", Params, |params| params) => (Ordered, Enter, false, false, Unavailable, Captured, None);
    Invite("invite", Params, |params| params) => (Ordered, Enter, false, false, Signed, Captured, None);
    Entrances("entrances", Params, |params| params) => (Read, Exempt, false, false, Allowed, Captured, None);
    JoinCheck("join_check", Params, |params| params) => (Ordered, Exempt, true, false, Allowed, Captured, None);
    Join("join", Params, |params| params) => (Ordered, Enter, true, false, Signed, Captured, None);
    JoinStatus("join_status", Params, |params| params) => (Ordered, Exempt, true, false, JoinStatus, Captured, None);
    JoinWithdraw("join_withdraw", Params, |params| params) => (Ordered, Enter, true, false, Signed, Captured, None);
    Approve("approve", Params, |params| params) => (Ordered, Enter, false, false, Signed, Captured, None);
    Revoke("revoke", Params, |params| params) => (Ordered, Enter, false, true, Signed, ReturnedPrincipal, None);
    Grant("grant", Params, |params| params) => (Ordered, Enter, false, true, Signed, ReturnedPrincipal, None);
    Veto("veto", Params, |params| params) => (Ordered, Enter, false, true, Signed, ReturnedPrincipal, None);
    ApproveRecovery("approve_recovery", Params, |params| params) => (Ordered, Enter, false, true, Signed, Captured, None);
    RecoveryState("recovery_state", Params, |params| params) => (Ordered, Exempt, true, false, Signed, Captured, None);
    Recover("recover", Params, |params| params) => (Ordered, Enter, true, false, Signed, ReturnedPrincipal, None);
    NodeJoinStatus("node_join_status", Params, |params| params) => (Ordered, Exempt, false, false, Allowed, Captured, None);
    NodeJoinCheck("node_join_check", Params, |params| params) => (Ordered, Enter, true, false, Allowed, Captured, None);
    NodeJoin("node_join", Params, |params| params) => (Ordered, Enter, true, false, Unavailable, Captured, None);
    NodeJoinCancel("node_join_cancel", Params, |params| params) => (Ordered, Enter, true, false, Allowed, Captured, None);
    Rejoin("rejoin", Params, |params| params) => (Ordered, Enter, true, false, Allowed, Captured, None);
}

pub(crate) fn client_policy(input: &ClientInput) -> ClientPolicy {
    input
        .as_ref()
        .map_or(REJECTED_CLIENT, ClientCommand::policy)
}
pub(crate) fn assemble_client(
    name: std::result::Result<ClientName, CommandRejection>,
    params: Params,
) -> ClientInput {
    name.map(|name| name.assemble(params))
}
pub(crate) fn decode_client(method: &str, params: Params) -> ClientInput {
    assemble_client(ClientName::decode(method), params)
}

macro_rules! business_commands {
    ($( $variant:ident($wire:literal) => ($lane:ident, $consistency:ident, $effect:ident); )*) => {
        #[derive(Debug, Clone)]
        pub(crate) enum BusinessCommand { $( $variant(BusinessParams), )* }
        impl BusinessCommand {
            fn decode(name: &str, params: BusinessParams) -> BusinessInput {
                match name { $( $wire => Ok(Self::$variant(params)), )*
                    _ => Err(BusinessRejection::UnsupportedName { params }), }
            }
            pub(crate) fn params(&self) -> &BusinessParams {
                match self { $( Self::$variant(params) => params, )* }
            }
            pub(crate) fn params_mut(&mut self) -> &mut BusinessParams {
                match self { $( Self::$variant(params) => params, )* }
            }
            pub(crate) fn lane(&self) -> Lane {
                match self { $( Self::$variant(_) => Lane::$lane, )* }
            }
            pub(crate) fn consistency(&self) -> ReadConsistency {
                match self { $( Self::$variant(_) => ReadConsistency::$consistency, )* }
            }
            pub(crate) fn effect(&self) -> SessionEffect {
                match self { $( Self::$variant(_) => SessionEffect::$effect, )* }
            }
        }
    }
}
business_commands! {
    Nodes("nodes") => (Read, AuthoritativeWriter, None);
    Conversations("conversations") => (Read, CommittedDisplay, None);
    CreateConversation("create_conversation") => (Ordered, AuthoritativeWriter, None);
    Conversation("conversation") => (Ordered, AuthoritativeWriter, Observe);
    ConversationHistory("conversation_history") => (Read, CommittedDisplay, None);
    OpenWorks("open_works") => (Read, CommittedDisplay, None);
    WorkDetail("work_detail") => (Read, AuthoritativeWriter, None);
    WorkspaceOverview("workspace_overview") => (Read, CommittedDisplay, None);
    WorkspaceCommand("workspace_command") => (Ordered, AuthoritativeWriter, None);
    SignedReceipt("signed_receipt") => (Read, AuthoritativeWriter, None);
    WorkspaceCommandReceipt("workspace_command_receipt") => (Read, AuthoritativeWriter, None);
    WorkspaceState("workspace_state") => (Read, CommittedDisplay, None);
    WorkspaceHistory("workspace_history") => (Read, CommittedDisplay, None);
    MessageSearch("message_search") => (Read, CommittedDisplay, None);
    Message("message") => (Ordered, AuthoritativeWriter, None);
    InputReceipt("input_receipt") => (Read, AuthoritativeWriter, None);
    ScopeContext("scope_context") => (Read, AuthoritativeWriter, None);
    SetOwner("set_owner") => (Ordered, AuthoritativeWriter, None);
    ReadArtifact("read_artifact") => (Read, AuthoritativeWriter, None);
    UploadAttachment("upload_attachment") => (Ordered, AuthoritativeWriter, None);
    InterruptWork("interrupt_work") => (Control, AuthoritativeWriter, None);
    CodexState("codex_state") => (Ordered, AuthoritativeWriter, None);
    Bots("bots") => (Read, AuthoritativeWriter, None);
    Questions("questions") => (Read, AuthoritativeWriter, None);
    Answer("answer") => (Ordered, AuthoritativeWriter, None);
    CreateBot("create_bot") => (Ordered, AuthoritativeWriter, None);
    DeleteBot("delete_bot") => (Ordered, AuthoritativeWriter, None);
    SetPrimaryBot("set_primary_bot") => (Ordered, AuthoritativeWriter, None);
    SetBotProject("set_bot_project") => (Ordered, AuthoritativeWriter, None);
    SetBotAvatar("set_bot_avatar") => (Ordered, AuthoritativeWriter, None);
    ControlSchedule("control_schedule") => (Ordered, AuthoritativeWriter, None);
    ControlMemory("control_memory") => (Ordered, AuthoritativeWriter, None);
    MemoryState("memory_state") => (Read, AuthoritativeWriter, None);
    CancelManager("cancel_manager") => (Control, AuthoritativeWriter, None);
    ReplyUnanswered("reply_unanswered") => (Ordered, AuthoritativeWriter, None);
    DismissUnanswered("dismiss_unanswered") => (Ordered, AuthoritativeWriter, None);
    Resolve("resolve") => (Ordered, AuthoritativeWriter, None);
    Notifications("notifications") => (Read, CommittedDisplay, None);
    DismissWork("dismiss_work") => (Ordered, AuthoritativeWriter, None);
    Ack("ack") => (Ordered, AuthoritativeWriter, None);
}

pub(crate) fn decode_business(params: Params) -> BusinessInput {
    let mut value = params.into_value();
    let method = value
        .as_object_mut()
        .expect("Params is an object")
        .remove("method");
    let params = BusinessParams(value);
    let Some(Value::String(method)) = method else {
        return Err(BusinessRejection::MethodField {
            error: BusinessMethodError::Required,
            params,
        });
    };
    if method.trim().is_empty() || method.len() > 65536 {
        return Err(BusinessRejection::MethodField {
            error: BusinessMethodError::Invalid,
            params,
        });
    }
    BusinessCommand::decode(&method, params)
}
pub(crate) fn business_params(input: &BusinessInput) -> &BusinessParams {
    match input {
        Ok(command) => command.params(),
        Err(
            BusinessRejection::MethodField { params, .. }
            | BusinessRejection::UnsupportedName { params },
        ) => params,
    }
}
pub(crate) fn business_params_mut(input: &mut BusinessInput) -> &mut BusinessParams {
    match input {
        Ok(command) => command.params_mut(),
        Err(
            BusinessRejection::MethodField { params, .. }
            | BusinessRejection::UnsupportedName { params },
        ) => params,
    }
}

#[cfg(test)]
pub(crate) fn business_fixture(value: Value) -> BusinessInput {
    decode_business(Params::new(value).expect("business fixture is an object"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn literal_business_policy_preserves_display_receipts_and_ordering() {
        for (method, lane, consistency, effect) in [
            (
                "conversations",
                Lane::Read,
                ReadConsistency::CommittedDisplay,
                SessionEffect::None,
            ),
            (
                "conversation_history",
                Lane::Read,
                ReadConsistency::CommittedDisplay,
                SessionEffect::None,
            ),
            (
                "open_works",
                Lane::Read,
                ReadConsistency::CommittedDisplay,
                SessionEffect::None,
            ),
            (
                "workspace_overview",
                Lane::Read,
                ReadConsistency::CommittedDisplay,
                SessionEffect::None,
            ),
            (
                "workspace_state",
                Lane::Read,
                ReadConsistency::CommittedDisplay,
                SessionEffect::None,
            ),
            (
                "workspace_history",
                Lane::Read,
                ReadConsistency::CommittedDisplay,
                SessionEffect::None,
            ),
            (
                "message_search",
                Lane::Read,
                ReadConsistency::CommittedDisplay,
                SessionEffect::None,
            ),
            (
                "notifications",
                Lane::Read,
                ReadConsistency::CommittedDisplay,
                SessionEffect::None,
            ),
            (
                "workspace_command_receipt",
                Lane::Read,
                ReadConsistency::AuthoritativeWriter,
                SessionEffect::None,
            ),
            (
                "signed_receipt",
                Lane::Read,
                ReadConsistency::AuthoritativeWriter,
                SessionEffect::None,
            ),
            (
                "input_receipt",
                Lane::Read,
                ReadConsistency::AuthoritativeWriter,
                SessionEffect::None,
            ),
            (
                "conversation",
                Lane::Ordered,
                ReadConsistency::AuthoritativeWriter,
                SessionEffect::Observe,
            ),
            (
                "interrupt_work",
                Lane::Control,
                ReadConsistency::AuthoritativeWriter,
                SessionEffect::None,
            ),
            (
                "cancel_manager",
                Lane::Control,
                ReadConsistency::AuthoritativeWriter,
                SessionEffect::None,
            ),
            (
                "nodes",
                Lane::Read,
                ReadConsistency::AuthoritativeWriter,
                SessionEffect::None,
            ),
            (
                "unknown",
                Lane::Ordered,
                ReadConsistency::AuthoritativeWriter,
                SessionEffect::None,
            ),
        ] {
            let input = decode_client("rpc", Params::new(json!({"method":method})).unwrap());
            let policy = client_policy(&input);
            assert_eq!(
                (policy.lane, policy.consistency, policy.effect),
                (lane, consistency, effect),
                "{method}"
            );
        }
    }

    #[test]
    fn literal_outer_policy_keeps_transition_and_delivery_sets_distinct() {
        for (method, guest, transition, delivery) in [
            ("info", true, false, DeliveryCheck::InfoNetwork),
            (
                "authenticate",
                true,
                false,
                DeliveryCheck::ReturnedPrincipal,
            ),
            ("recover", true, false, DeliveryCheck::ReturnedPrincipal),
            ("grant", false, true, DeliveryCheck::ReturnedPrincipal),
            ("revoke", false, true, DeliveryCheck::ReturnedPrincipal),
            ("veto", false, true, DeliveryCheck::ReturnedPrincipal),
            ("approve_recovery", false, true, DeliveryCheck::Captured),
            ("approve", false, false, DeliveryCheck::Captured),
            ("invite", false, false, DeliveryCheck::Captured),
            ("unknown", false, false, DeliveryCheck::Captured),
        ] {
            let policy = client_policy(&decode_client(method, Params::new(json!({})).unwrap()));
            assert_eq!(
                (policy.guest, policy.transition, policy.delivery),
                (guest, transition, delivery),
                "{method}"
            );
        }
    }

    #[test]
    fn business_method_rules_preserve_bytes_and_original_payloads() {
        for value in [json!({}), json!({"method":null}), json!({"method":7})] {
            assert!(matches!(
                business_fixture(value),
                Err(BusinessRejection::MethodField {
                    error: BusinessMethodError::Required,
                    ..
                })
            ));
        }
        for method in [
            "".to_owned(),
            " \t".to_owned(),
            "x".repeat(65537),
            "é".repeat(32769),
        ] {
            assert!(matches!(
                business_fixture(json!({"method":method})),
                Err(BusinessRejection::MethodField {
                    error: BusinessMethodError::Invalid,
                    ..
                })
            ));
        }
        for method in [" nodes ".to_owned(), "x".repeat(65536), "é".repeat(32768)] {
            assert!(matches!(
                business_fixture(json!({"method":method})),
                Err(BusinessRejection::UnsupportedName { .. })
            ));
        }
        let original = json!({"method":"message","attestation":{"device_id":"signed"}});
        let input = business_fixture(
            json!({"method":"input_receipt","original_input":original,"command":{"method":"nested"}}),
        );
        assert_eq!(
            business_params(&input).value(),
            &json!({"original_input":original,"command":{"method":"nested"}})
        );
        let input =
            decode_client("info", Params::new(json!({"method":"unrelated"})).unwrap()).unwrap();
        let ClientCommand::Info(params) = input else {
            panic!("info must decode as Info")
        };
        assert_eq!(params.into_value(), json!({"method":"unrelated"}));
    }
}
