//! Intent tools for the chat Agent. Protocol identities belong to durable local calls.
use super::*;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Clone, Debug, Serialize, Deserialize)]
struct PreparedCall {
    conversation_id: String,
    name: String,
    intent: Value,
    requests: Vec<OperationRequest>,
}

fn call_intent(args: &Value) -> Result<Value> {
    let mut intent = args
        .as_object()
        .context("tool arguments must be an object")?
        .clone();
    intent.remove("call_ref");
    Ok(Value::Object(intent))
}

fn same_request(actual: &OperationRequest, expected: &OperationRequest) -> bool {
    let mut expected = expected.clone();
    expected.resources = actual.resources.clone();
    expected.manager_authority = actual.manager_authority.clone();
    actual == &expected
}

impl Runtime {
    pub(super) async fn agent_tool(
        self: &Arc<Self>,
        access: Access,
        name: &str,
        args: Value,
        call_id: &str,
    ) -> Result<Value> {
        let scope = access
            .conversation_id
            .as_deref()
            .context("missing tool scope")?;
        let original = if matches!(name, "continue_execution" | "stop_execution") {
            Some(self.agent_work(&access, field(&args, "work_ref")?).await?)
        } else {
            None
        };
        let write = !matches!(
            name,
            "context" | "nodes" | "execution_status" | "call_lookup"
        );
        let _lease = self
            .manager_tool_lease(
                &access,
                write,
                original
                    .as_ref()
                    .and_then(|request| request.task_id.as_deref()),
            )
            .await?;
        match name {
            "nodes" => self.rpc_inner(access, json!({"method":"nodes"})).await,
            "context" => self.agent_context(&access, &args).await,
            "execution_status" => self.agent_status(&access, args["work_ref"].as_str()).await,
            "call_lookup" => {
                let reference = field(&args, "call_ref")?;
                let prepared = self
                    .load_agent_call(&access, reference)
                    .await?
                    .context("call reference not found in this invocation")?;
                self.run_agent_call(&access, reference, &prepared, false)
                    .await
            }
            "command" | "delegate" | "continue_execution" | "stop_execution" => {
                let reference = args["call_ref"].as_str().unwrap_or(call_id);
                ensure!(
                    !reference.is_empty()
                        && reference.len() <= 256
                        && !reference.chars().any(char::is_control),
                    "a bounded stable call reference is required"
                );
                let intent = call_intent(&args)?;
                let prepared =
                    if let Some(prepared) = self.load_agent_call(&access, reference).await? {
                        ensure!(
                            prepared.name == name && prepared.intent == intent,
                            "call reference reused with different tool arguments"
                        );
                        prepared
                    } else {
                        self.ensure_agent_calls_confirmed(&access).await?;
                        let prepared = self
                            .prepare_agent_call(&access, name, intent, original)
                            .await?;
                        let manager = access
                            .manager
                            .as_ref()
                            .context("write tools require a live management invocation")?;
                        let index_key = format!("agent-calls:v1:{}", manager.attempt_id);
                        let mut store = self.store.lock().await;
                        let mut calls = store
                            .local_value(&access.network_id, &index_key)?
                            .unwrap_or(json!({}));
                        if let Some(existing) = calls.get(reference) {
                            let existing: PreparedCall = serde_json::from_value(existing.clone())?;
                            ensure!(
                                existing.name == prepared.name
                                    && existing.intent == prepared.intent
                                    && existing.conversation_id == scope,
                                "call reference reused with different tool arguments"
                            );
                            existing
                        } else {
                            calls[reference] = json!(prepared);
                            store.local_set(&access.network_id, &index_key, &calls)?;
                            prepared
                        }
                    };
                self.run_agent_call(&access, reference, &prepared, true)
                    .await
            }
            _ => bail!("unknown or unavailable tool"),
        }
    }

    async fn load_agent_call(
        &self,
        access: &Access,
        reference: &str,
    ) -> Result<Option<PreparedCall>> {
        let manager = access
            .manager
            .as_ref()
            .context("tool call lookup requires a management invocation")?;
        let scope = access
            .conversation_id
            .as_deref()
            .context("missing tool scope")?;
        let calls = self
            .store
            .lock()
            .await
            .local_value(
                &access.network_id,
                &format!("agent-calls:v1:{}", manager.attempt_id),
            )?
            .unwrap_or(json!({}));
        let call: Option<PreparedCall> = calls
            .get(reference)
            .cloned()
            .map(serde_json::from_value)
            .transpose()?;
        ensure!(
            call.as_ref()
                .is_none_or(|call| call.conversation_id == scope),
            "tool call belongs to another conversation"
        );
        Ok(call)
    }

    async fn ensure_agent_calls_confirmed(self: &Arc<Self>, access: &Access) -> Result<()> {
        let manager = access
            .manager
            .as_ref()
            .context("write tools require a management invocation")?;
        let calls = self
            .store
            .lock()
            .await
            .local_value(
                &access.network_id,
                &format!("agent-calls:v1:{}", manager.attempt_id),
            )?
            .unwrap_or(json!({}));
        for value in calls
            .as_object()
            .context("invalid prepared call index")?
            .values()
        {
            let call: PreparedCall = serde_json::from_value(value.clone())?;
            for request in &call.requests {
                let (dispatch, operation) = self.agent_receipt(access, request).await?;
                ensure!(
                    operation
                        .as_ref()
                        .is_none_or(|record| record.state != OperationState::Unknown),
                    "an earlier call has an unknown result; query its original call_ref before starting new work"
                );
                ensure!(
                    dispatch.is_none() || operation.is_some(),
                    "an earlier dispatch has no confirmed receipt; query its original call_ref instead of issuing a new call"
                );
            }
        }
        Ok(())
    }

    async fn prepare_agent_call(
        self: &Arc<Self>,
        access: &Access,
        name: &str,
        intent: Value,
        original: Option<OperationRequest>,
    ) -> Result<PreparedCall> {
        let scope = access
            .conversation_id
            .as_deref()
            .context("missing tool scope")?;
        let basis = self.store.lock().await.heads(&access.network_id, scope)?;
        let make = |target: String,
                    session: Option<String>,
                    revision: u64,
                    task: Option<String>,
                    binding,
                    action: Value| OperationRequest {
            operation_id: uuid::Uuid::new_v4().to_string(),
            network_id: access.network_id.clone(),
            conversation_id: scope.into(),
            task_id: task,
            target_node: target,
            session_id: session,
            expected_revision: revision,
            basis_heads: basis.clone(),
            action,
            resources: vec![],
            binding,
            manager_authority: None,
        };
        let requests = match name {
            "command" | "delegate" => {
                let mut resolved = intent.clone();
                if resolved.get("cwd").is_none() || resolved.get("target_node").is_none() {
                    if let Some(project) = self.agent_project(access).await? {
                        if resolved.get("cwd").is_none() {
                            resolved["cwd"] = json!(project.cwd);
                        }
                        if resolved.get("target_node").is_none() {
                            resolved["target_node"] = json!(project.target_node);
                        }
                    }
                }
                ensure!(
                    resolved["cwd"]
                        .as_str()
                        .is_some_and(|cwd| !cwd.trim().is_empty()),
                    "choose an allowed cwd from nodes, or configure a Bot project before executing"
                );
                let target = self.agent_target(access, &resolved, name).await?;
                let cwd = field(&resolved, "cwd")?;
                if name == "command" {
                    let maximum_timeout = self
                        .config
                        .timeout_secs
                        .min(crate::command::MAX_TIMEOUT_SECS)
                        .max(1);
                    let timeout = intent["timeout_secs"]
                        .as_u64()
                        .unwrap_or(maximum_timeout)
                        .clamp(1, maximum_timeout);
                    vec![make(
                        target,
                        None,
                        0,
                        None,
                        None,
                        json!({"type":"command","command":field(&intent,"command")?,"cwd":cwd,"timeout_secs":timeout}),
                    )]
                } else {
                    let executor = intent["executor"].as_str().unwrap_or("codex");
                    let session = Some(uuid::Uuid::new_v4().to_string());
                    vec![
                        make(
                            target.clone(),
                            session.clone(),
                            0,
                            None,
                            None,
                            json!({"type":"open","executor":executor,"cwd":cwd}),
                        ),
                        make(
                            target,
                            session,
                            1,
                            None,
                            None,
                            json!({"type":"send","text":field(&intent,"text")?}),
                        ),
                    ]
                }
            }
            "continue_execution" | "stop_execution" => {
                let original = original.context("original work is required")?;
                let (_, receipt) = self.agent_receipt(access, &original).await?;
                let receipt = receipt.context("original work has no confirmed execution receipt; reconcile it before controlling a session")?;
                ensure!(
                    !matches!(
                        receipt.state,
                        OperationState::Unknown | OperationState::Rejected
                    ),
                    "reconcile the original execution before controlling it"
                );
                let session_id = original
                    .session_id
                    .as_deref()
                    .context("this work has no executor session")?;
                let status = self
                    .session_fact(&access.network_id, &original.target_node, session_id)
                    .await?;
                let session: ExecutionSession = serde_json::from_value(status["session"].clone())?;
                ensure!(
                    session.network_id == access.network_id
                        && session.target_node == original.target_node
                        && session.session_id == session_id,
                    "executor returned a different session"
                );
                ensure!(
                    session.state != "unknown",
                    "reconcile the original execution before continuing or stopping it"
                );
                let kind: ExecutorKind = serde_json::from_value(json!(session.executor))?;
                let active = session.state == "running";
                let action = if name == "stop_execution" || active {
                    let action_name = if name == "stop_execution" {
                        "interrupt"
                    } else {
                        "steer"
                    };
                    ensure!(
                        kind.capabilities().contains(&action_name),
                        "this executor does not support {action_name}"
                    );
                    let turn = active_turn(&status).context(
                        "there is no confirmed active turn; query status before retrying",
                    )?;
                    if action_name == "interrupt" {
                        json!({"type":"interrupt","turn_id":turn})
                    } else {
                        json!({"type":"steer","turn_id":turn,"text":field(&intent,"text")?})
                    }
                } else {
                    json!({"type":"send","text":field(&intent,"text")?})
                };
                vec![make(
                    original.target_node,
                    Some(session_id.into()),
                    session.revision,
                    original.task_id,
                    original.binding,
                    action,
                )]
            }
            _ => bail!("unsupported write tool"),
        };
        Ok(PreparedCall {
            conversation_id: scope.into(),
            name: name.into(),
            intent,
            requests,
        })
    }

    async fn agent_project(&self, access: &Access) -> Result<Option<BotProject>> {
        let scope = access
            .conversation_id
            .as_deref()
            .context("missing tool scope")?;
        let manager = access
            .manager
            .as_ref()
            .context("project preferences require a live management invocation")?;
        let store = self.store.lock().await;
        let job = store
            .scheduler_job(
                &access.network_id,
                scope,
                &manager.trigger,
                &manager.target_node,
            )?
            .context("management job is missing")?;
        let context = store.context(&access.network_id, scope, &job.basis_heads)?;
        Ok(crate::scheduler::context_bot(&context)?.and_then(|bot| bot.project))
    }

    async fn agent_target(
        self: &Arc<Self>,
        access: &Access,
        intent: &Value,
        tool: &str,
    ) -> Result<String> {
        let nodes = self
            .rpc_inner(access.clone(), json!({"method":"nodes"}))
            .await?;
        let cwd = std::path::Path::new(field(intent, "cwd")?);
        ensure!(
            cwd.is_absolute(),
            "cwd must be an absolute allowed directory"
        );
        let mut candidates = nodes["nodes"]
            .as_array()
            .context("invalid node directory")?
            .iter()
            .filter(|node| {
                node["online"] == true
                    && node["revoked"] == false
                    && node["roles"]
                        .as_array()
                        .is_some_and(|roles| roles.contains(&json!("executor")))
                    && intent["target_node"]
                        .as_str()
                        .is_none_or(|target| node["node_id"].as_str() == Some(target))
                    && node["capabilities"]["roots"]
                        .as_array()
                        .is_some_and(|roots| {
                            roots
                                .iter()
                                .filter_map(Value::as_str)
                                .any(|root| cwd.starts_with(root))
                        })
                    && (tool == "command"
                        || node["capabilities"]["executors"]
                            .as_array()
                            .is_some_and(|executors| {
                                executors.contains(&json!(
                                    intent["executor"].as_str().unwrap_or("codex")
                                ))
                            }))
            })
            .collect::<Vec<_>>();
        candidates.sort_by_key(|node| {
            (
                node["local"] != true,
                node["node_id"].as_str().unwrap_or(""),
            )
        });
        Ok(field(
            candidates.first().context(
                "no authorized reachable execution Node supports this directory and executor",
            )?,
            "node_id",
        )?
        .into())
    }

    async fn agent_work(&self, access: &Access, reference: &str) -> Result<OperationRequest> {
        let scope = access
            .conversation_id
            .as_deref()
            .context("missing tool scope")?;
        let context = self
            .store
            .lock()
            .await
            .context(&access.network_id, scope, &[])?;
        let mut original = None;
        for event in context.events {
            if event.network_id != access.network_id || event.scope_id != scope {
                continue;
            }
            if let EventBody::DispatchRecorded {
                operation_id,
                target_node,
                session_id,
                task_id,
                action,
            } = event.body
            {
                if operation_id != reference {
                    continue;
                }
                let request: OperationRequest = serde_json::from_value(action)?;
                ensure!(
                    request.operation_id == operation_id
                        && request.target_node == target_node
                        && request.session_id == session_id
                        && request.task_id == task_id
                        && request.network_id == access.network_id
                        && request.conversation_id == scope,
                    "dispatch metadata does not match the requested work"
                );
                ensure!(
                    original.as_ref().is_none_or(|old| old == &request),
                    "work has conflicting original dispatches"
                );
                original = Some(request);
            }
        }
        original.context("work reference is outside this conversation or has not been dispatched")
    }

    async fn agent_receipt(
        self: &Arc<Self>,
        access: &Access,
        expected: &OperationRequest,
    ) -> Result<(Option<OperationRequest>, Option<OperationRecord>)> {
        let original = match self.agent_work(access, &expected.operation_id).await {
            Ok(original) => Some(original),
            Err(error)
                if error.to_string()
                    == "work reference is outside this conversation or has not been dispatched" =>
            {
                None
            }
            Err(error) => return Err(error),
        };
        if let Some(original) = &original {
            ensure!(
                same_request(original, expected),
                "prepared request differs from its original dispatch"
            );
        }
        let operation = if expected.target_node == self.identity.node_id()? {
            self.store
                .lock()
                .await
                .operation(&access.network_id, &expected.operation_id)?
        } else {
            serde_json::from_value(
                self.read_peer(
                    &access.network_id,
                    &expected.target_node,
                    json!({"type":"operation","operation_id":expected.operation_id}),
                )
                .await?["operation"]
                    .clone(),
            )?
        };
        if let Some(operation) = &operation {
            let signed = original
                .as_ref()
                .context("execution receipt has no original conversation dispatch")?;
            let mut normalized = signed.clone();
            normalized.resources = operation.request.resources.clone();
            ensure!(
                operation.request == normalized,
                "execution receipt differs from the original dispatch"
            );
        }
        Ok((original, operation))
    }

    async fn run_agent_call(
        self: &Arc<Self>,
        access: &Access,
        reference: &str,
        prepared: &PreparedCall,
        dispatch: bool,
    ) -> Result<Value> {
        ensure!(
            access.conversation_id.as_deref() == Some(prepared.conversation_id.as_str()),
            "prepared call belongs to another conversation"
        );
        let work_ref = &prepared
            .requests
            .first()
            .context("prepared call has no request")?
            .operation_id;
        let mut operations = Vec::new();
        let mut ready = true;
        for request in &prepared.requests {
            let (original, mut record) = self.agent_receipt(access, request).await?;
            if record.is_none() && original.is_none() && dispatch && ready {
                self.dispatch(access.clone(), request.clone()).await?;
                record = self.agent_receipt(access, request).await?.1;
            }
            if dispatch
                && ((prepared.name == "delegate" && request.action["type"] == "open")
                    || prepared.name == "command")
            {
                let deadline = tokio::time::Instant::now()
                    + Duration::from_secs(self.config.timeout_secs.min(20));
                while record.as_ref().is_some_and(|record| {
                    matches!(
                        record.state,
                        OperationState::Accepted | OperationState::Running
                    )
                }) && tokio::time::Instant::now() < deadline
                {
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    record = self.agent_receipt(access, request).await?.1;
                }
            }
            let state = record
                .as_ref()
                .map(|record| serde_json::to_value(&record.state))
                .transpose()?
                .unwrap_or_else(|| {
                    json!(if original.is_some() {
                        "unknown"
                    } else {
                        "prepared"
                    })
                });
            operations
                .push(json!({"operation_id":request.operation_id,"state":state,"receipt":record}));
            ready = record
                .as_ref()
                .is_some_and(|record| record.state == OperationState::Succeeded);
            if !ready {
                break;
            }
        }
        Ok(
            json!({"call_ref":reference,"work_ref":work_ref,"tool":prepared.name,"operations":operations,"all_dispatched":operations.len()==prepared.requests.len() && operations.iter().all(|operation| operation["state"] != "prepared"),"retry":"Reuse this call_ref with identical arguments. If a dispatch is unknown, query call_lookup and reconcile; never allocate another call to resend it."}),
        )
    }

    async fn agent_status(
        self: &Arc<Self>,
        access: &Access,
        reference: Option<&str>,
    ) -> Result<Value> {
        if let Some(reference) = reference {
            let request = self.agent_work(access, reference).await?;
            let (_, record) = self.agent_receipt(access, &request).await?;
            let session = match request.session_id.as_deref() {
                Some(session)
                    if record
                        .as_ref()
                        .is_some_and(|record| record.state != OperationState::Rejected) =>
                {
                    Some(
                        self.session_fact(&access.network_id, &request.target_node, session)
                            .await?,
                    )
                }
                _ => None,
            };
            return Ok(json!({"work_ref":reference,"operation":record,"session":session}));
        }
        let scope = access
            .conversation_id
            .as_deref()
            .context("missing tool scope")?;
        let context = self
            .store
            .lock()
            .await
            .context(&access.network_id, scope, &[])?;
        let terminal = context
            .events
            .iter()
            .filter_map(|event| match &event.body {
                EventBody::ExecutionObserved {
                    operation_id,
                    target_node,
                    status,
                    ..
                } if event.scope_id == scope
                    && event.author == *target_node
                    && matches!(
                        status.as_str(),
                        "succeeded" | "failed" | "rejected" | "completed" | "interrupted"
                    ) =>
                {
                    Some(operation_id.clone())
                }
                _ => None,
            })
            .collect::<BTreeSet<_>>();
        let mut work = BTreeMap::new();
        for event in context.events {
            if event.scope_id != scope {
                continue;
            }
            if let EventBody::DispatchRecorded {
                operation_id,
                action,
                ..
            } = event.body
            {
                let request: OperationRequest = serde_json::from_value(action)?;
                if request.network_id == access.network_id
                    && request.conversation_id == scope
                    && !terminal.contains(&operation_id)
                {
                    work.insert(operation_id.clone(), json!({"work_ref":operation_id,"target_node":request.target_node,"session_id":request.session_id,"action":request.action["type"]}));
                }
            }
        }
        Ok(json!({"work":work.into_values().collect::<Vec<_>>()}))
    }

    async fn agent_context(&self, access: &Access, args: &Value) -> Result<Value> {
        let scope = access
            .conversation_id
            .as_deref()
            .context("missing tool scope")?;
        let context = self
            .store
            .lock()
            .await
            .context(&access.network_id, scope, &[])?;
        let offset = args["offset"].as_u64().unwrap_or(0).min(usize::MAX as u64) as usize;
        let limit = args["limit"].as_u64().unwrap_or(20).clamp(1, 100) as usize;
        if let Some(hash) = args["blob_hash"].as_str() {
            ensure!(
                context
                    .events
                    .iter()
                    .filter(|event| event.scope_id == scope)
                    .flat_map(|event| &event.blobs)
                    .any(|blob| blob.hash == hash),
                "blob outside this conversation"
            );
            let bytes = self
                .store
                .lock()
                .await
                .get_blob(&access.network_id, hash)?
                .context("blob not available")?;
            let end = offset.saturating_add(limit * 1024).min(bytes.len());
            ensure!(offset <= end, "invalid blob offset");
            return Ok(
                json!({"hash":hash,"offset":offset,"data_hex":hex::encode(&bytes[offset..end]),"next_offset":(end<bytes.len()).then_some(end),"size":bytes.len()}),
            );
        }
        let events = context
            .events
            .into_iter()
            .filter(|event| event.scope_id == scope)
            .map(|event| {
                json!({
                    "author":event.author,"created_at_ms":event.created_at_ms,
                    "body":crate::scheduler::agent_event_body(&event.body),"blobs":event.blobs,
                })
            })
            .collect::<Vec<_>>();
        let end = offset.saturating_add(limit).min(events.len());
        ensure!(offset <= end, "invalid event offset");
        Ok(json!({"events":&events[offset..end],"next_offset":(end<events.len()).then_some(end)}))
    }
}

fn active_turn(status: &Value) -> Option<&str> {
    if let Some(turn) = status["actual"]["last_turn"].as_object() {
        if matches!(
            turn.get("status").and_then(Value::as_str),
            Some("inProgress" | "in_progress" | "running")
        ) {
            return turn.get("id").and_then(Value::as_str);
        }
    }
    let thread = &status["actual"]["thread"];
    let turns = thread["thread"]["turns"]
        .as_array()
        .or_else(|| thread["turns"].as_array())?;
    turns.iter().rev().find(|turn| {
        matches!(
            turn["status"].as_str(),
            Some("inProgress" | "in_progress" | "running")
        )
    })?["id"]
        .as_str()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn command_preparation_bounds_default_and_requested_timeouts_before_admission()
    -> Result<()> {
        let dir = crate::runtime::tests::fixture()?;
        let node = Arc::new(Runtime::open(&dir).await?);
        let cwd = dir.canonicalize()?;
        let network = crate::runtime::tests::conversation(&node).await?;
        assert_eq!(node.config.timeout_secs, 600);
        let access = Access {
            network_id: network.clone(),
            human: false,
            conversation_id: Some("chat".into()),
            manager: None,
        };
        for (requested, expected) in [
            (None, crate::command::MAX_TIMEOUT_SECS),
            (Some(600), crate::command::MAX_TIMEOUT_SECS),
            (Some(10), 10),
            (Some(0), 1),
        ] {
            let mut intent =
                json!({"command":"pwd","cwd":cwd,"target_node":node.identity.node_id()?});
            if let Some(requested) = requested {
                intent["timeout_secs"] = json!(requested);
            }
            let prepared = node
                .prepare_agent_call(&access, "command", intent, None)
                .await?;
            assert_eq!(prepared.requests.len(), 1);
            let spec = node.command_spec(&prepared.requests[0].action)?;
            assert_eq!(spec.timeout_secs, expected);
            spec.validate()?;
            assert!(prepared.requests[0].session_id.is_none());
        }
        let invalid = json!({"type":"command","command":"pwd","cwd":cwd,"timeout_secs":crate::command::MAX_TIMEOUT_SECS+1});
        assert!(node.command_spec(&invalid).is_err());
        assert!(node.store.lock().await.operations(&network)?.is_empty());
        drop(node);
        std::fs::remove_dir_all(dir)?;
        Ok(())
    }
}
