//! Execution evidence shared by the Node tool handlers and their transport clients.
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// The tool names and effects offered by the Node, also used to bind wire receipts.
#[macro_export]
macro_rules! coordination_tools {
    ($define:ident) => {
        $define! {
            WorkspaceState => ("workspace_state", Read),
            WorkspaceHistory => ("workspace_history", Read),
            WorkspaceCommand => ("workspace_command", Work),
            Search => ("tool_search", Read),
            Describe => ("tool_describe", Read),
            Call => ("tool_call", Read),
            Nodes => ("nodes", Read),
            Context => ("context", Read),
            DispatchWork => ("dispatch_work", Work),
            WorkStatus => ("work_status", Read),
            InterruptWork => ("interrupt_work", Work),
            CallLookup => ("call_lookup", Read),
            UpdateProfile => ("update_profile", Settings),
            SaveSkill => ("save_skill", Settings),
            RemoveSkill => ("remove_skill", Settings),
            SaveSchedule => ("save_schedule", Settings),
            RemoveSchedule => ("remove_schedule", Settings),
            ReadSkill => ("read_skill", Read),
            Remember => ("remember", Settings),
            Forget => ("forget", Settings),
            Recall => ("recall", Read),
            Remind => ("remind", Wake),
            MessageBot => ("message_bot", Wake),
            FetchUrl => ("fetch_url", Read),
            ListBots => ("list_bots", Read),
            ReadBot => ("read_bot", Read),
            CreateBot => ("create_bot", Settings),
            ConfigureBot => ("configure_bot", Settings),
            DeleteBot => ("delete_bot", Settings),
            HandOverSession => ("hand_over_session", Work),
            ListSessions => ("list_sessions", Read),
            ProposeProviderConfig => ("propose_provider_config", Work),
            ApplyProviderConfig => ("apply_provider_config", Work),
            TrackWork => ("track_work", Work),
            BindWork => ("bind_work", Work),
            WaitWork => ("wait_work", Read),
            HistorySearch => ("history_search", Read),
            HistoryRead => ("history_read", Read),
        }
    };
}

macro_rules! tool_effect {
    (@read Read) => { true };
    (@read $write:ident) => { false };
    ($($variant:ident => ($name:literal, $effect:ident)),+ $(,)?) => {
        fn read_tool(name: &str) -> Option<bool> {
            match name { $($name => Some(tool_effect!(@read $effect)),)+ _ => None }
        }
    };
}
coordination_tools!(tool_effect);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Execution {
    Read,
    Applied,
    NotStarted,
    Started,
    Completed,
    Pending,
    Unknown,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Approval {
    NotRequired,
    Pending,
    Allowed,
    Denied,
    Unknown,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EffectReceipt {
    pub execution: Execution,
    pub approval: Approval,
}

impl Execution {
    fn wire(self, reference: Option<&Value>) -> (&'static str, &'static str, &'static str) {
        let (execution, confirmation, retry) = match self {
            Execution::Read => ("read", "confirmed", "none"),
            Execution::Applied => ("applied", "confirmed", "none"),
            Execution::NotStarted => ("not_started", "confirmed", "none"),
            Execution::Started => ("started", "confirmed", "none"),
            Execution::Completed => ("completed", "confirmed", "none"),
            Execution::Pending => ("unknown", "pending", "lookup_same_ref"),
            Execution::Unknown => ("unknown", "unknown", "lookup_same_ref"),
        };
        let retry = if reference
            .and_then(Value::as_str)
            .is_some_and(|id| vibeface_protocol::validate_id(id).is_ok())
        {
            retry
        } else {
            "none"
        };
        (execution, confirmation, retry)
    }
    pub fn with(self, approval: Approval) -> EffectReceipt {
        EffectReceipt {
            execution: self,
            approval,
        }
    }
    pub fn result(self, value: Value) -> ToolOutcome {
        self.with(Approval::NotRequired).result(value)
    }
}

impl EffectReceipt {
    pub fn from_error(error: &anyhow::Error) -> Option<Self> {
        error
            .downcast_ref::<EffectFailure>()
            .map(|failure| failure.receipt)
    }
    pub fn result(self, value: Value) -> ToolOutcome {
        ToolOutcome {
            value,
            receipt: self,
            reference: None,
        }
    }
    pub fn record(self, tool: &str, reference: Option<&Value>, stage: &str) -> Value {
        let (execution, confirmation, retry) = self.execution.wire(reference);
        json!({"tool":tool,"call_ref":reference,"stage":stage,"execution":execution,
            "confirmation":confirmation,"approval":self.approval,"retry":retry})
    }
    pub fn failure(self, error: anyhow::Error) -> anyhow::Error {
        EffectFailure {
            receipt: self,
            error,
        }
        .into()
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct ToolOutcome {
    pub value: Value,
    pub receipt: EffectReceipt,
    reference: Option<Value>,
}
impl std::borrow::Borrow<Value> for ToolOutcome {
    fn borrow(&self) -> &Value {
        &self.value
    }
}

impl crate::agent::DispatchOutput for ToolOutcome {
    fn missing_after_track(error: anyhow::Error) -> anyhow::Error {
        error
    }
}

impl ToolOutcome {
    pub fn for_reference(mut self, reference: &str) -> Self {
        self.reference = Some(json!(reference));
        self
    }
    pub fn reference(args: &Value) -> Option<&Value> {
        args.get("call_ref")
            .or_else(|| args["arguments"].get("call_ref"))
    }
    pub fn into_response(self, tool: &str, args: &Value) -> Value {
        let stage = if tool == "call_lookup" {
            "lookup"
        } else {
            "result"
        };
        let mut value = self.value;
        if !value.is_object() {
            value = json!({"value":value});
        }
        value["call_record"] = self.receipt.record(
            tool,
            Self::reference(args).or(self.reference.as_ref()),
            stage,
        );
        value
    }
}

#[derive(Debug)]
struct EffectFailure {
    receipt: EffectReceipt,
    error: anyhow::Error,
}
impl std::fmt::Display for EffectFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.error.fmt(f)
    }
}
impl std::error::Error for EffectFailure {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.error.source()
    }
}
pub fn uncertain(error: anyhow::Error) -> anyhow::Error {
    Execution::Unknown.with(Approval::Unknown).failure(error)
}

/// Validate the Node's complete receipt for this invocation before exposing its payload.
pub fn validate_response(name: &str, args: &Value, response: &Value, success: bool) -> Result<()> {
    let record = response
        .get("call_record")
        .context("node tool response has no call_record")?;
    let stage = record["stage"]
        .as_str()
        .context("invalid tool receipt stage")?;
    let nested = name == "tool_call" && stage != "validation";
    let target = if nested {
        args["tool"]
            .as_str()
            .context("invalid nested tool identity")?
    } else {
        name
    };
    ensure!(
        record["tool"].as_str() == Some(target),
        "tool receipt belongs to another tool"
    );
    let target_args = if nested { &args["arguments"] } else { args };
    let reference = if success && target == "work_status" && target_args["work_id"].is_string() {
        ensure!(
            response["work_id"] == target_args["work_id"]
                && response["call_ref"]
                    .as_str()
                    .is_some_and(|id| vibeface_protocol::validate_id(id).is_ok()),
            "invalid work status reference"
        );
        response.get("call_ref")
    } else {
        ToolOutcome::reference(args)
    };
    ensure!(
        record.get("call_ref") == Some(reference.unwrap_or(&Value::Null)),
        "tool receipt has a different reference"
    );
    let approval: Approval = serde_json::from_value(record["approval"].clone())
        .context("invalid tool receipt approval")?;
    let facts = [
        Execution::Read,
        Execution::Applied,
        Execution::NotStarted,
        Execution::Started,
        Execution::Completed,
        Execution::Pending,
        Execution::Unknown,
    ]
    .into_iter()
    .find(|execution| {
        let (state, confirmation, retry) = execution.wire(reference);
        record["execution"] == state
            && record["confirmation"] == confirmation
            && record["retry"] == retry
    })
    .context("invalid execution, confirmation or retry in tool receipt")?;
    if success {
        if matches!(
            approval,
            Approval::Denied | Approval::Pending | Approval::Allowed
        ) {
            ensure!(
                matches!(target, "propose_provider_config" | "apply_provider_config"),
                "unexpected approval decision in successful tool receipt"
            );
        }
        match target {
            "apply_provider_config" => {
                ensure!(
                    target_args["question_id"]
                        .as_str()
                        .is_some_and(|id| !id.is_empty())
                        && response["question_id"] == target_args["question_id"],
                    "provider receipt belongs to a different question"
                );
                ensure!(
                    matches!(
                        (response["status"].as_str(), facts, approval),
                        (Some("applied"), Execution::Applied, Approval::Allowed)
                            | (Some("declined"), Execution::NotStarted, Approval::Denied)
                    ),
                    "provider application receipt contradicts the decision"
                );
            }
            "bind_work" => ensure!(
                target_args["work_id"]
                    .as_str()
                    .is_some_and(|id| !id.is_empty())
                    && response["work_id"] == target_args["work_id"],
                "binding receipt belongs to a different work"
            ),
            _ => {}
        }
        let read = read_tool(target).context("unknown tool in successful receipt")?;
        if target == "call_lookup" {
            ensure!(
                stage == "lookup" && facts != Execution::Read,
                "invalid lookup receipt"
            );
        } else {
            ensure!(
                stage == "result" && read == (facts == Execution::Read),
                "tool receipt contradicts the tool effect"
            );
        }
    } else {
        let code = match stage {
            "validation" => "invalid_tool_arguments",
            "authorization" => "tool_not_authorized",
            "execution" => "tool_execution_failed",
            _ => anyhow::bail!("invalid failure stage in tool receipt"),
        };
        ensure!(
            response["error"]
                .as_str()
                .is_some_and(|text| !text.is_empty())
                && response["code"] == code,
            "invalid tool failure envelope"
        );
        ensure!(
            facts != Execution::Read && (stage == "execution" || facts == Execution::NotStarted),
            "invalid execution for failure stage"
        );
        ensure!(
            stage == "validation" || read_tool(target).is_some(),
            "unknown tool outside validation"
        );
        ensure!(
            stage == "execution" || matches!(approval, Approval::Unknown | Approval::Denied),
            "invalid approval before execution"
        );
        if read_tool(target) == Some(true) {
            ensure!(
                matches!(facts, Execution::NotStarted | Execution::Unknown),
                "read failure cannot confirm a write"
            );
        }
    }
    ensure!(
        match approval {
            Approval::Denied | Approval::Pending => facts == Execution::NotStarted,
            Approval::Allowed =>
                matches!(target, "propose_provider_config" | "apply_provider_config")
                    && matches!(
                        facts,
                        Execution::NotStarted | Execution::Applied | Execution::Unknown
                    ),
            Approval::Unknown => facts != Execution::Read,
            Approval::NotRequired => true,
        },
        "tool approval contradicts execution"
    );
    Ok(())
}
