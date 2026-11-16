use super::{Action, Operation, Reconciliation};
use anyhow::{Result, ensure};
use serde::Deserialize;
use serde_json::{Map, Value};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ActionPolicy {
    MemberWrite,
    ManagingMemberWrite,
    NodeControl,
    EnableLocalAccess,
    SafetyStop,
}

impl Action {
    pub(super) fn policy(&self) -> ActionPolicy {
        match self {
            Self::Input { .. }
            | Self::Workspace { .. }
            | Self::Memory { .. }
            | Self::Schedule { .. }
            | Self::SetOwner { .. }
            | Self::DismissWork { .. }
            | Self::Unanswered { .. }
            | Self::Answer { .. }
            | Self::CreateBot { .. }
            | Self::DeleteBot { .. }
            | Self::SetPrimaryBot { .. }
            | Self::SetBotProject { .. }
            | Self::SetBotAvatar { .. }
            | Self::AckNotification { .. }
            | Self::ResolveCheck { .. } => ActionPolicy::MemberWrite,
            Self::Delivery(_) => ActionPolicy::ManagingMemberWrite,
            Self::NodeSettings(_)
            | Self::ProviderSettings(_)
            | Self::Triage(_)
            | Self::Relay { .. }
            | Self::CreateDecisionToken { .. }
            | Self::RevokeDecisionToken { .. }
            | Self::ConfirmWritable
            | Self::TestWebhook
            | Self::LocalAccess { enabled: false } => ActionPolicy::NodeControl,
            Self::LocalAccess { enabled: true } => ActionPolicy::EnableLocalAccess,
            Self::StopReply { .. } | Self::StopWork { .. } => ActionPolicy::SafetyStop,
        }
    }
}

// These views parse policy fields only. Business payloads retain their existing validators.
#[derive(Deserialize)]
struct StoredRequest {
    method: Method,
    params: Map<String, Value>,
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum Method {
    Rpc,
    Delivery,
    LocalAccess,
    ConfigureNode,
    ConfigureProvider,
    ConfigureTriage,
    ConfigureRelay,
    CreateDecisionToken,
    RevokeDecisionToken,
    ConfirmWritable,
    TestWebhook,
}

#[derive(Deserialize)]
#[serde(tag = "method", rename_all = "snake_case")]
enum RpcPolicy {
    Message {},
    WorkspaceCommand {},
    ControlMemory {},
    ControlSchedule {},
    SetOwner {},
    DismissWork {},
    ReplyUnanswered {},
    DismissUnanswered {},
    Answer {},
    CreateBot {},
    DeleteBot {},
    SetPrimaryBot {},
    SetBotProject {},
    SetBotAvatar {},
    Ack {},
    Resolve {},
    CancelManager {
        conversation_id: String,
        trigger: String,
    },
    InterruptWork {
        conversation_id: String,
        work_id: String,
    },
}

#[derive(Deserialize)]
struct DeliveryPolicy {
    op: DeliveryOperation,
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum DeliveryOperation {
    Configure,
    Prepare,
    Confirm,
    Accept,
    Reconcile,
    ConfirmBatch,
    AcceptBatch,
    FlushBudget,
}

#[derive(Deserialize)]
struct LocalAccessPolicy {
    enabled: bool,
}

pub(super) fn stored_policy(operation: &Operation) -> Result<ActionPolicy> {
    let parse = || -> Result<ActionPolicy> {
        let request: StoredRequest = serde_json::from_slice(&operation.bytes)?;
        Ok(match request.method {
            Method::Rpc => {
                match serde_json::from_value::<RpcPolicy>(Value::Object(request.params))? {
                    RpcPolicy::Message {}
                    | RpcPolicy::WorkspaceCommand {}
                    | RpcPolicy::ControlMemory {}
                    | RpcPolicy::ControlSchedule {}
                    | RpcPolicy::SetOwner {}
                    | RpcPolicy::DismissWork {}
                    | RpcPolicy::ReplyUnanswered {}
                    | RpcPolicy::DismissUnanswered {}
                    | RpcPolicy::Answer {}
                    | RpcPolicy::CreateBot {}
                    | RpcPolicy::DeleteBot {}
                    | RpcPolicy::SetPrimaryBot {}
                    | RpcPolicy::SetBotProject {}
                    | RpcPolicy::SetBotAvatar {}
                    | RpcPolicy::Ack {}
                    | RpcPolicy::Resolve {} => ActionPolicy::MemberWrite,
                    RpcPolicy::CancelManager {
                        conversation_id,
                        trigger,
                    } => {
                        ensure!(
                            matches!(&operation.reconciliation,
                        Reconciliation::StopReply { conversation_id: original, trigger: target }
                        if original == &conversation_id && target == &trigger),
                            "stored stop does not match its original reply"
                        );
                        ActionPolicy::SafetyStop
                    }
                    RpcPolicy::InterruptWork {
                        conversation_id,
                        work_id,
                    } => {
                        ensure!(
                            matches!(&operation.reconciliation,
                        Reconciliation::StopWork { conversation_id: original, work_id: target }
                        if original == &conversation_id && target == &work_id),
                            "stored stop does not match its original work"
                        );
                        ActionPolicy::SafetyStop
                    }
                }
            }
            Method::Delivery => {
                let _ = serde_json::from_value::<DeliveryPolicy>(Value::Object(request.params))?.op;
                ActionPolicy::ManagingMemberWrite
            }
            Method::LocalAccess => {
                let access: LocalAccessPolicy =
                    serde_json::from_value(Value::Object(request.params))?;
                if access.enabled {
                    ActionPolicy::EnableLocalAccess
                } else {
                    ActionPolicy::NodeControl
                }
            }
            Method::ConfigureNode
            | Method::ConfigureProvider
            | Method::ConfigureTriage
            | Method::ConfigureRelay
            | Method::CreateDecisionToken
            | Method::RevokeDecisionToken
            | Method::ConfirmWritable
            | Method::TestWebhook => ActionPolicy::NodeControl,
        })
    };
    parse().map_err(|_| anyhow::anyhow!("invalid stored operation policy"))
}
