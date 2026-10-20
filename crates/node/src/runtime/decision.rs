//! LANE-B STUB: lane A replaces this file with the real `send_decision` (resume, steer or
//! start, read back by `clientUserMessageId`). Only the request shape and path are shared.
use super::Runtime;
use anyhow::{Result, bail};
use serde_json::Value;
use std::sync::Arc;

pub(crate) struct DecisionRequest {
    pub thread_id: String,
    pub text: String,
    pub decision_id: String,
}

pub(crate) async fn send_decision(_node: &Arc<Runtime>, request: DecisionRequest) -> Result<Value> {
    let _ = (request.thread_id, request.text, request.decision_id);
    bail!("send_decision is not available in this build")
}
