//! A completed source import is scoped to one immutable Task business wire.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use vibeface_secure::authorization::{
    FreshnessReceipt, ObjectHash, Signed,
    task_business_policy::{
        TaskBusinessPolicyHeader, TaskBusinessPolicyPage, TaskBusinessPolicyWitness,
        TaskBusinessWire, VerifiedTaskBusinessSource, receipt::TaskBusinessChallenge,
    },
};

mod ingress;
mod original;
mod receipt;
mod source;
mod storage;
pub(crate) use ingress::{TaskBusinessPolicyImportContext, check_task_business_policy_view_in};
pub(crate) use original::{
    TaskBusinessOriginalRecovery, bind_task_business_original_in,
    check_task_business_original_source_in,
};
pub(crate) use source::{TaskBusinessPolicySourceContext, TaskBusinessSourceChunk};

#[derive(Serialize, Deserialize)]
#[serde(
    tag = "stage",
    content = "request",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub(crate) enum TaskBusinessRequest {
    SourceBegin {
        header: Signed<TaskBusinessPolicyHeader>,
    },
    SourcePage {
        page: Signed<TaskBusinessPolicyPage>,
    },
    SourceFinish {
        witness: Signed<TaskBusinessPolicyWitness>,
    },
    Business(TaskBusinessWire),
    Status {
        wire: TaskBusinessWire,
    },
    Receipts {
        source_context: ObjectHash,
        business_wire_hash: ObjectHash,
        challenge_hash: ObjectHash,
        receipts: Vec<Signed<FreshnessReceipt>>,
    },
}

#[derive(Serialize, Deserialize)]
#[serde(
    tag = "stage",
    content = "response",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub(crate) enum TaskBusinessResponse {
    SourceBegun {
        context_hash: ObjectHash,
    },
    SourcePageAccepted {
        context_hash: ObjectHash,
        next_page_index: u64,
        complete: bool,
    },
    SourceFinished {
        context_hash: ObjectHash,
    },
    Challenge {
        source_context: ObjectHash,
        business_wire_hash: ObjectHash,
        challenge: TaskBusinessChallenge,
    },
    Business(Value),
    Status(Value),
}

#[derive(Clone)]
pub(crate) struct TaskBusinessPolicySession {
    source: VerifiedTaskBusinessSource,
}

impl TaskBusinessPolicySession {
    pub(crate) fn source(&self) -> &VerifiedTaskBusinessSource {
        &self.source
    }
}
