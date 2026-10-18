//! Intent tools for the chat Agent. Protocol identities belong to durable local calls.
use super::*;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

const AGENT_BLOB_BYTES: usize = 2 * MAX_BLOB_BYTES;
const AGENT_BLOB_OBJECTS: usize = 4;
const AGENT_BLOB_IDLE_TTL: Duration = Duration::from_secs(300);

#[derive(Clone, PartialEq, Eq, Hash)]
pub(super) struct AgentBlobKey {
    attempt_id: Option<String>,
    network_id: String,
    scope_id: String,
    hash: String,
    expected_size: u64,
}

pub(super) struct AgentBlobSnapshot {
    bytes: Vec<u8>,
    last_used: Instant,
}

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
        let deadline = tokio::time::Instant::now()
            + Duration::from_secs(self.config.timeout_secs.clamp(1, 20));
        let scope = access
            .conversation_id
            .as_deref()
            .context("missing tool scope")?;
        let original = if matches!(name, "continue_execution" | "stop_execution") {
            Some(
                tokio::time::timeout_at(
                    deadline,
                    self.agent_work(&access, field(&args, "work_ref")?),
                )
                .await
                .context(
                    "tool preparation timed out before dispatch; keep the original call_ref",
                )??,
            )
        } else {
            None
        };
        let write = !matches!(
            name,
            "context" | "nodes" | "execution_status" | "call_lookup"
        );
        let _lease = tokio::time::timeout_at(
            deadline,
            self.manager_tool_lease(
                &access,
                write,
                original
                    .as_ref()
                    .and_then(|request| request.task_id.as_deref()),
            ),
        )
        .await
        .context("tool admission timed out before dispatch; keep the original call_ref")??;
        match name {
            "nodes" => {
                tokio::time::timeout_at(deadline, self.rpc_inner(access, json!({"method":"nodes"})))
                    .await
                    .context("tool node lookup timed out")?
            }
            "context" => tokio::time::timeout_at(deadline, self.agent_context(&access, &args))
                .await
                .context("tool context lookup timed out")?,
            "execution_status" => tokio::time::timeout_at(
                deadline,
                self.agent_status(&access, args["work_ref"].as_str()),
            )
            .await
            .context("tool status lookup timed out; keep the original work_ref")?,
            "call_lookup" => {
                let reference = field(&args, "call_ref")?;
                let prepared =
                    tokio::time::timeout_at(deadline, self.load_agent_call(&access, reference))
                        .await
                        .with_context(|| {
                            format!("tool lookup timed out; keep call_ref {reference}")
                        })??;
                let prepared = prepared.context("call reference not found in this invocation")?;
                self.run_agent_call(&access, reference, &prepared, false, deadline)
                    .await
            }
            "command" | "delegate" | "continue_execution" | "stop_execution" => {
                let sdk_reference = if call_id.len() > 256 || call_id.chars().any(char::is_control)
                {
                    format!("sdk:{}", crate::crypto::digest(call_id.as_bytes()))
                } else {
                    call_id.to_owned()
                };
                let reference = args["call_ref"].as_str().unwrap_or(&sdk_reference);
                ensure!(
                    !reference.is_empty()
                        && reference.len() <= 256
                        && !reference.chars().any(char::is_control),
                    "a bounded stable call reference is required"
                );
                let intent = call_intent(&args)?;
                let prepared = tokio::time::timeout_at(deadline, async {
                    let prepared = if let Some(prepared) = self.load_agent_call(&access, reference).await? {
                        ensure!(prepared.name == name && prepared.intent == intent, "call reference reused with different tool arguments");
                        prepared
                    } else {
                        self.ensure_agent_calls_confirmed(&access).await?;
                        let prepared = self.prepare_agent_call(&access, name, intent, original).await?;
                        let manager = access.manager.as_ref().context("write tools require a live management invocation")?;
                        let index_key = format!("agent-calls:v1:{}", manager.attempt_id);
                        let mut store = self.store.lock().await;
                        let mut calls = store.local_value(&access.network_id, &index_key)?.unwrap_or(json!({}));
                        if let Some(existing) = calls.get(reference) {
                            let existing: PreparedCall = serde_json::from_value(existing.clone())?;
                            ensure!(existing.name == prepared.name && existing.intent == prepared.intent && existing.conversation_id == scope, "call reference reused with different tool arguments");
                            existing
                        } else {
                            calls[reference] = json!(prepared);
                            store.local_set(&access.network_id, &index_key, &calls)?;
                            prepared
                        }
                    };
                    Ok::<PreparedCall, anyhow::Error>(prepared)
                }).await.with_context(|| format!("tool preparation timed out before dispatch; keep call_ref {reference} and query call_lookup before retrying"))??;
                self.run_agent_call(&access, reference, &prepared, true, deadline)
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
                if (resolved.get("cwd").is_none() || resolved.get("target_node").is_none())
                    && let Some(project) = self.agent_project(access).await?
                {
                    if resolved.get("cwd").is_none() {
                        resolved["cwd"] = json!(project.cwd);
                    }
                    if resolved.get("target_node").is_none() {
                        resolved["target_node"] = json!(project.target_node);
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
                        .clamp(1, crate::command::MAX_TIMEOUT_SECS);
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
                        .is_none_or(|target| node["peer_id"].as_str() == Some(target))
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
                node["peer_id"].as_str().unwrap_or(""),
            )
        });
        Ok(field(
            candidates.first().context(
                "no authorized reachable execution Node supports this directory and executor",
            )?,
            "peer_id",
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
        let operation = if expected.target_node == self.identity.peer_id()? {
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
        deadline: tokio::time::Instant,
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
            if tokio::time::Instant::now() >= deadline {
                operations.push(
                    json!({"operation_id":request.operation_id,"state":"unknown","receipt":null}),
                );
                break;
            }
            let (original, mut record) = match tokio::time::timeout_at(
                deadline,
                self.agent_receipt(access, request),
            )
            .await
            {
                Ok(result) => result?,
                Err(_) => {
                    operations.push(json!({"operation_id":request.operation_id,"state":"unknown","receipt":null}));
                    break;
                }
            };
            if record.is_none() && original.is_none() && dispatch && ready {
                if tokio::time::Instant::now() >= deadline {
                    operations.push(json!({"operation_id":request.operation_id,"state":"unknown","receipt":null}));
                    break;
                }
                match tokio::time::timeout_at(
                    deadline,
                    self.dispatch(access.clone(), request.clone()),
                )
                .await
                {
                    Ok(result) => {
                        result?;
                    }
                    Err(_) => {
                        operations.push(json!({"operation_id":request.operation_id,"state":"unknown","receipt":null}));
                        break;
                    }
                }
                record = match tokio::time::timeout_at(
                    deadline,
                    self.agent_receipt(access, request),
                )
                .await
                {
                    Ok(result) => result?.1,
                    Err(_) => {
                        operations.push(json!({"operation_id":request.operation_id,"state":"unknown","receipt":null}));
                        break;
                    }
                };
            }
            if dispatch
                && ((prepared.name == "delegate" && request.action["type"] == "open")
                    || prepared.name == "command")
            {
                while record.as_ref().is_some_and(|record| {
                    matches!(
                        record.state,
                        OperationState::Accepted | OperationState::Running
                    )
                }) && tokio::time::Instant::now() < deadline
                {
                    if tokio::time::timeout_at(
                        deadline,
                        tokio::time::sleep(Duration::from_millis(100)),
                    )
                    .await
                    .is_err()
                    {
                        break;
                    }
                    match tokio::time::timeout_at(deadline, self.agent_receipt(access, request))
                        .await
                    {
                        Ok(result) => record = result?.1,
                        Err(_) => break,
                    }
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
            json!({"call_ref":reference,"work_ref":work_ref,"tool":prepared.name,"operations":operations,"all_dispatched":operations.len()==prepared.requests.len() && operations.iter().all(|operation| operation["receipt"].is_object()),"retry":"Reuse this call_ref with identical arguments. If a dispatch is unknown, query call_lookup and reconcile; never allocate another call to resend it."}),
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
        let offset = args["offset"].as_u64().unwrap_or(0).min(usize::MAX as u64) as usize;
        let limit = args["limit"].as_u64().unwrap_or(20).clamp(1, 100) as usize;
        if let Some(hash) = args["blob_hash"].as_str() {
            return self.agent_blob_page(access, hash, offset, limit).await;
        }
        let context = self
            .store
            .lock()
            .await
            .context(&access.network_id, scope, &[])?;
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

    async fn agent_blob_page(
        &self,
        access: &Access,
        hash: &str,
        offset: usize,
        limit_kib: usize,
    ) -> Result<Value> {
        let scope = access
            .conversation_id
            .as_deref()
            .context("missing tool scope")?;
        let attempt = access
            .manager
            .as_ref()
            .map(|manager| manager.attempt_id.clone());
        let scope_access = self.require_scope(access, scope).await;
        let mut snapshots = self.agent_blobs.lock().await;
        let now = Instant::now();
        snapshots.retain(|_, snapshot| {
            now.saturating_duration_since(snapshot.last_used) < AGENT_BLOB_IDLE_TTL
        });
        let result = async {
            scope_access?;
            let store = self.store.lock().await;
            let local = self.identity.peer_id()?;
            let current = self.identities.public_state()?;
            ensure!(current.genesis.network_id == access.network_id, "wrong personal network");
            current.project()?.authorize_node(&local, None)?;
            store.authorize_node(&access.network_id, &local, None)?;
            ensure!(store.has_conversation(&access.network_id, scope)?, "conversation not found");
            Self::check_manager_permission(&store, access, false, None)?;
            let expected = store.context_blob_ref(&access.network_id, scope, hash)?;
            let size = usize::try_from(expected.size)?;
            ensure!(size <= MAX_BLOB_BYTES, "blob size limit exceeded");
            ensure!(offset <= size, "invalid blob offset");
            let key = AgentBlobKey {
                attempt_id: attempt.clone(),
                network_id: access.network_id.clone(),
                scope_id: scope.into(),
                hash: hash.into(),
                expected_size: expected.size,
            };
            if offset == 0 {
                drop(snapshots.remove(&key));
            }
            let count = snapshots.len();
            let used: usize = snapshots.values().map(|snapshot| snapshot.bytes.len()).sum();
            if let std::collections::hash_map::Entry::Vacant(entry) = snapshots.entry(key.clone()) {
                ensure!(
                    count < AGENT_BLOB_OBJECTS
                        && used.checked_add(size).is_some_and(|total| total <= AGENT_BLOB_BYTES),
                    Busy("Agent attachment snapshots are full; finish an existing read before retrying".into())
                );
                let bytes = store.get_blob_for_ref(&access.network_id, &expected)?
                    .context("blob not available")?;
                ensure!(bytes.len() == size, "blob size mismatch");
                entry.insert(AgentBlobSnapshot { bytes, last_used: now });
            }
            let snapshot = snapshots.get_mut(&key).unwrap();
            let end = offset.saturating_add(limit_kib.clamp(1, 100) * 1024).min(snapshot.bytes.len());
            let page = json!({"hash":hash,"offset":offset,"data_hex":hex::encode(&snapshot.bytes[offset..end]),"next_offset":(end<snapshot.bytes.len()).then_some(end),"size":snapshot.bytes.len()});
            snapshot.last_used = Instant::now();
            if end == snapshot.bytes.len() {
                drop(snapshots.remove(&key));
            }
            Ok(page)
        }.await;
        if result.is_err() {
            snapshots.retain(|key, _| {
                !(key.attempt_id == attempt
                    && key.network_id == access.network_id
                    && key.scope_id == scope
                    && key.hash == hash)
            });
        }
        result
    }

    pub(super) async fn clear_agent_blob_attempt(&self, attempt_id: &str) {
        self.agent_blobs
            .lock()
            .await
            .retain(|key, _| key.attempt_id.as_deref() != Some(attempt_id));
    }
}

fn active_turn(status: &Value) -> Option<&str> {
    if let Some(turn) = status["actual"]["last_turn"].as_object()
        && matches!(
            turn.get("status").and_then(Value::as_str),
            Some("inProgress" | "in_progress" | "running")
        )
    {
        return turn.get("id").and_then(Value::as_str);
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

    async fn attachment(
        node: &Runtime,
        access: &Access,
        bytes: &[u8],
        name: &str,
    ) -> Result<BlobRef> {
        let blob = BlobRef {
            hash: crypto::digest(bytes),
            size: bytes.len() as u64,
        };
        node.store
            .lock()
            .await
            .put_blob(&access.network_id, &blob, bytes)?;
        node.append_blobs(
            &access.network_id,
            access.conversation_id.as_deref().unwrap(),
            EventBody::MessagePosted {
                message_id: name.into(),
                role: "user".into(),
                text: name.into(),
                scheduler_node: None,
            },
            vec![blob.clone()],
        )
        .await?;
        Ok(blob)
    }

    async fn blob_fixture() -> Result<(
        PathBuf,
        Arc<Runtime>,
        Access,
        LocalIdentity,
        BlobRef,
        Vec<u8>,
    )> {
        let dir = crate::runtime::tests::fixture_under("network-reliability")?;
        let node = Arc::new(Runtime::open(&dir).await?);
        let owner = LocalIdentity::generate();
        let state = owner.sign_network_fixture(
            1,
            vec![node.identity.certificate(node.config.roles.clone())?],
        )?;
        let network = node.install_membership(state).await?.network_id;
        node.append(
            &network,
            "chat",
            EventBody::ConversationCreated {
                title: "blob pages".into(),
            },
        )
        .await?;
        let access = Access {
            network_id: network,
            human: false,
            conversation_id: Some("chat".into()),
            manager: None,
        };
        let bytes = (0..2500)
            .map(|index| (index % 251) as u8)
            .collect::<Vec<_>>();
        let blob = attachment(&node, &access, &bytes, "attachment").await?;
        Ok((dir, node, access, owner, blob, bytes))
    }

    async fn managed_blob_access(node: &Runtime, access: &Access, name: &str) -> Result<Access> {
        let target = node.identity.peer_id()?;
        let scope = access.conversation_id.as_deref().unwrap();
        let (trigger, attempt) = {
            let mut store = node.store.lock().await;
            let (message, _) = store.accept_message(
                &node.identity,
                &access.network_id,
                scope,
                EventBody::MessagePosted {
                    message_id: name.into(),
                    role: "user".into(),
                    text: name.into(),
                    scheduler_node: Some(target.clone()),
                },
                vec![],
                now_ms(),
                &target,
                &target,
                false,
            )?;
            let claimed = store
                .claim_next(
                    &access.network_id,
                    scope,
                    &target,
                    "blob-page-test",
                    "test-sdk",
                )?
                .context("management job was not claimed")?;
            let attempt = claimed
                .job
                .state
                .attempt()
                .context("manager attempt missing")?
                .id
                .clone();
            store.mark_started(&access.network_id, scope, &target, &message.hash, &attempt)?;
            (message.hash, attempt)
        };
        node.register_manager_attempt(name.into(), access.clone(), &attempt, &trigger, false)
            .await?;
        node.agent_tokens
            .lock()
            .await
            .get(name)
            .cloned()
            .context("management token missing")
    }

    #[tokio::test]
    async fn agent_blob_pages_keep_verified_bytes_until_refresh_and_reclaim_on_last_page()
    -> Result<()> {
        let (dir, node, access, _, blob, bytes) = blob_fixture().await?;
        let first = node
            .agent_tool(
                access.clone(),
                "context",
                json!({"blob_hash":blob.hash,"offset":0,"limit":1}),
                "first-callback",
            )
            .await?;
        assert_eq!(first["hash"], blob.hash);
        assert_eq!(first["offset"], 0);
        assert_eq!(first["size"], bytes.len());
        assert_eq!(first["next_offset"], 1024);
        assert_eq!(
            hex::decode(first["data_hex"].as_str().unwrap())?,
            bytes[..1024]
        );
        let corrupt = vec![0xee_u8; bytes.len()];
        rusqlite::Connection::open(dir.join("node.sqlite3"))?.execute(
            "UPDATE blobs SET content=? WHERE network_id=? AND hash=?",
            rusqlite::params![corrupt, access.network_id, blob.hash],
        )?;
        assert!(
            node.store
                .lock()
                .await
                .get_blob(&access.network_id, &blob.hash)
                .is_err()
        );
        let second = node
            .agent_tool(
                access.clone(),
                "context",
                json!({"blob_hash":blob.hash,"offset":1024,"limit":1}),
                "another-callback",
            )
            .await?;
        assert_eq!(second["next_offset"], 2048);
        assert_eq!(
            hex::decode(second["data_hex"].as_str().unwrap())?,
            bytes[1024..2048]
        );
        assert!(
            node.agent_context(
                &access,
                &json!({"blob_hash":blob.hash,"offset":0,"limit":1})
            )
            .await
            .is_err()
        );
        assert!(node.agent_blobs.lock().await.is_empty());
        node.store
            .lock()
            .await
            .put_blob(&access.network_id, &blob, &bytes)?;
        let mut received = Vec::new();
        for offset in [0, 1024, 2048] {
            let page = node
                .agent_context(
                    &access,
                    &json!({"blob_hash":blob.hash,"offset":offset,"limit":1}),
                )
                .await?;
            assert_eq!(page["offset"], offset);
            assert_eq!(page["size"], bytes.len());
            assert_eq!(
                page["next_offset"],
                if offset == 2048 {
                    Value::Null
                } else {
                    json!(offset + 1024)
                }
            );
            received.extend(hex::decode(page["data_hex"].as_str().unwrap())?);
        }
        assert_eq!(received, bytes);
        assert!(node.agent_blobs.lock().await.is_empty());
        drop(node);
        std::fs::remove_dir_all(dir)?;
        Ok(())
    }

    #[tokio::test]
    async fn agent_blob_snapshots_isolate_scope_attempt_and_drain_on_finish_or_fence() -> Result<()>
    {
        let (dir, node, access, _, blob, bytes) = blob_fixture().await?;
        let first = managed_blob_access(&node, &access, "first-manager").await?;
        node.agent_tool(
            first.clone(),
            "context",
            json!({"blob_hash":blob.hash,"limit":1}),
            "page",
        )
        .await?;
        node.append(
            &access.network_id,
            "other",
            EventBody::ConversationCreated {
                title: "other".into(),
            },
        )
        .await?;
        let mut other = access.clone();
        other.conversation_id = Some("other".into());
        assert!(
            node.agent_context(&other, &json!({"blob_hash":blob.hash,"limit":1}))
                .await
                .is_err()
        );
        assert_eq!(node.agent_blobs.lock().await.len(), 1);
        attachment(&node, &other, &bytes, "other-attachment").await?;
        let second = managed_blob_access(&node, &other, "second-manager").await?;
        node.agent_tool(
            second.clone(),
            "context",
            json!({"blob_hash":blob.hash,"limit":1}),
            "page",
        )
        .await?;
        assert_eq!(node.agent_blobs.lock().await.len(), 2);
        let mut wrong_attempt = first.clone();
        wrong_attempt.manager.as_mut().unwrap().attempt_id =
            second.manager.as_ref().unwrap().attempt_id.clone();
        assert!(
            node.agent_context(
                &wrong_attempt,
                &json!({"blob_hash":blob.hash,"offset":1024,"limit":1})
            )
            .await
            .is_err()
        );
        assert_eq!(node.agent_blobs.lock().await.len(), 2);
        node.finish_manager_attempt(&first.manager.as_ref().unwrap().attempt_id)
            .await;
        assert_eq!(node.agent_blobs.lock().await.len(), 1);
        assert!(
            node.agent_tool(
                first,
                "context",
                json!({"blob_hash":blob.hash,"offset":1024,"limit":1}),
                "finished"
            )
            .await
            .is_err()
        );
        let page = node
            .agent_tool(
                second.clone(),
                "context",
                json!({"blob_hash":blob.hash,"offset":1024,"limit":1}),
                "continued",
            )
            .await?;
        assert_eq!(
            hex::decode(page["data_hex"].as_str().unwrap())?,
            bytes[1024..2048]
        );
        node.fence_manager_attempt(&second.manager.as_ref().unwrap().attempt_id)
            .await?;
        assert!(node.agent_blobs.lock().await.is_empty());
        assert!(
            node.agent_tool(
                second.clone(),
                "context",
                json!({"blob_hash":blob.hash,"offset":2048,"limit":1}),
                "fenced"
            )
            .await
            .is_err()
        );
        node.finish_manager_attempt(&second.manager.as_ref().unwrap().attempt_id)
            .await;
        drop(node);
        std::fs::remove_dir_all(dir)?;
        Ok(())
    }

    #[tokio::test]
    async fn agent_blob_snapshot_admission_reclaims_idle_and_invalid_reads() -> Result<()> {
        let (dir, node, access, _, blob, bytes) = blob_fixture().await?;
        let mut blobs = vec![blob];
        for index in 1..=AGENT_BLOB_OBJECTS {
            let mut content = bytes.clone();
            content[0] = index as u8;
            blobs.push(attachment(&node, &access, &content, &format!("attachment-{index}")).await?);
        }
        for blob in &blobs[..AGENT_BLOB_OBJECTS] {
            node.agent_context(&access, &json!({"blob_hash":blob.hash,"limit":1}))
                .await?;
        }
        assert_eq!(node.agent_blobs.lock().await.len(), AGENT_BLOB_OBJECTS);
        let next = &blobs[AGENT_BLOB_OBJECTS];
        let error = node
            .agent_context(&access, &json!({"blob_hash":next.hash,"limit":1}))
            .await
            .unwrap_err();
        assert!(error.downcast_ref::<Busy>().is_some());
        assert_eq!(node.agent_blobs.lock().await.len(), AGENT_BLOB_OBJECTS);
        for snapshot in node.agent_blobs.lock().await.values_mut() {
            snapshot.last_used = Instant::now() - AGENT_BLOB_IDLE_TTL;
        }
        node.agent_context(&access, &json!({"blob_hash":next.hash,"limit":1}))
            .await?;
        assert_eq!(node.agent_blobs.lock().await.len(), 1);
        assert!(
            node.agent_context(
                &access,
                &json!({"blob_hash":next.hash,"offset":bytes.len()+1,"limit":1})
            )
            .await
            .is_err()
        );
        assert!(node.agent_blobs.lock().await.is_empty());
        drop(node);
        std::fs::remove_dir_all(dir)?;
        Ok(())
    }

    #[tokio::test]
    async fn agent_blob_snapshot_byte_budget_releases_the_final_page() -> Result<()> {
        let (dir, node, access, _, small, _) = blob_fixture().await?;
        let mut blobs = Vec::new();
        for marker in [0xa1, 0xa2] {
            let bytes = vec![marker; MAX_BLOB_BYTES];
            let blob = attachment(&node, &access, &bytes, &format!("large-{marker}")).await?;
            let page = node
                .agent_context(&access, &json!({"blob_hash":blob.hash,"limit":1}))
                .await?;
            assert_eq!(page["size"], MAX_BLOB_BYTES);
            blobs.push(blob);
        }
        let used: usize = node
            .agent_blobs
            .lock()
            .await
            .values()
            .map(|snapshot| snapshot.bytes.len())
            .sum();
        assert_eq!(used, AGENT_BLOB_BYTES);
        let error = node
            .agent_context(&access, &json!({"blob_hash":small.hash,"limit":1}))
            .await
            .unwrap_err();
        assert!(error.downcast_ref::<Busy>().is_some());
        let last = node
            .agent_context(
                &access,
                &json!({"blob_hash":blobs[0].hash,"offset":MAX_BLOB_BYTES-1024,"limit":1}),
            )
            .await?;
        assert_eq!(last["next_offset"], Value::Null);
        assert_eq!(
            hex::decode(last["data_hex"].as_str().unwrap())?,
            vec![0xa1; 1024]
        );
        node.agent_context(&access, &json!({"blob_hash":small.hash,"limit":1}))
            .await?;
        let snapshots = node.agent_blobs.lock().await;
        assert_eq!(snapshots.len(), 2);
        assert_eq!(
            snapshots
                .values()
                .map(|snapshot| snapshot.bytes.len())
                .sum::<usize>(),
            MAX_BLOB_BYTES + small.size as usize
        );
        drop(snapshots);
        drop(node);
        std::fs::remove_dir_all(dir)?;
        Ok(())
    }

    #[tokio::test]
    async fn agent_blob_pages_recheck_membership_after_waiting_for_cache() -> Result<()> {
        let (dir, node, access, owner, blob, _) = blob_fixture().await?;
        node.agent_context(&access, &json!({"blob_hash":blob.hash,"limit":1}))
            .await?;
        let snapshots = node.agent_blobs.lock().await;
        let args = json!({"blob_hash":blob.hash,"offset":1024,"limit":1});
        let mut request = Box::pin(node.agent_context(&access, &args));
        std::future::poll_fn(|cx| {
            assert!(std::future::Future::poll(request.as_mut(), cx).is_pending());
            std::task::Poll::Ready(())
        })
        .await;
        let mut certificate = node.identity.certificate(node.config.roles.clone())?;
        certificate.revoked = true;
        node.install_membership(owner.sign_network_fixture(2, vec![certificate])?)
            .await?;
        drop(snapshots);
        assert!(request.await.is_err());
        assert!(node.agent_blobs.lock().await.is_empty());
        drop(node);
        std::fs::remove_dir_all(dir)?;
        Ok(())
    }

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
                json!({"command":"pwd","cwd":cwd,"target_node":node.identity.peer_id()?});
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
