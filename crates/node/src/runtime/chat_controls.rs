use super::*;
use crate::binding::SignedBinding;

const ARTIFACT_PAGE: usize = 65_536;

fn session_cache_key(target: &str, session: &str) -> String {
    format!(
        "chat-session:v1:{}",
        crypto::digest(&serde_json::to_vec(&(target, session)).expect("string tuple serializes"))
    )
}

fn control_key(operation: &str) -> String {
    format!("chat-control:v1:{operation}")
}

fn same_business_request(observed: &OperationRequest, original: &OperationRequest) -> bool {
    let mut admitted = original.clone();
    // Admission expands resource locks without changing the dispatched business request.
    admitted.resources = observed.resources.clone();
    observed == &admitted
}

fn prepared_request(
    store: &Store,
    network: &str,
    operation: Option<&str>,
    intent: &Value,
) -> Result<Option<OperationRequest>> {
    let Some(operation) = operation else {
        return Ok(None);
    };
    let Some(prepared) = store.local_value(network, &control_key(operation))? else {
        return Ok(None);
    };
    ensure!(
        prepared["intent"] == *intent,
        "operation ID reused with a different task control"
    );
    Ok(Some(serde_json::from_value(prepared["request"].clone())?))
}

fn bound_dispatches(context: &Context, proof: &SignedBinding) -> Result<Vec<OperationRequest>> {
    let binding = &proof.binding;
    let mut requests = Vec::new();
    for event in &context.events {
        if event.network_id != binding.network_id || event.scope_id != binding.conversation_id {
            continue;
        }
        let EventBody::DispatchRecorded {
            operation_id,
            task_id,
            target_node,
            session_id,
            action,
        } = &event.body
        else {
            continue;
        };
        let Ok(request) = serde_json::from_value::<OperationRequest>(action.clone()) else {
            continue;
        };
        if request.network_id != binding.network_id
            || request.conversation_id != binding.conversation_id
            || request.task_id.as_deref() != Some(binding.task_id.as_str())
            || request.target_node != binding.target_node
            || request.session_id != binding.session_id
            || request.binding.as_ref() != Some(proof)
        {
            continue;
        }
        ensure!(
            request.operation_id == *operation_id
                && request.task_id == *task_id
                && request.target_node == *target_node
                && request.session_id == *session_id,
            "dispatch metadata does not match its task request"
        );
        ensure!(
            context.events.iter().all(|other| {
                !matches!(&other.body, EventBody::DispatchRecorded { operation_id: id, .. } if id == operation_id)
                    || (other.network_id == event.network_id
                        && other.scope_id == event.scope_id
                        && other.body == event.body)
            }),
            "operation ID reused with different dispatched body"
        );
        if !requests
            .iter()
            .any(|old: &OperationRequest| old.operation_id == request.operation_id)
        {
            requests.push(request);
        }
    }
    Ok(requests)
}

fn matches_dispatch(requests: &[OperationRequest], operation: &OperationRecord) -> Result<bool> {
    let Some(request) = requests
        .iter()
        .find(|request| request.operation_id == operation.request.operation_id)
    else {
        return Ok(false);
    };
    ensure!(
        same_business_request(&operation.request, request),
        "operation ledger does not match its dispatched body"
    );
    Ok(true)
}

fn bound_operations(
    requests: &[OperationRequest],
    records: impl IntoIterator<Item = OperationRecord>,
) -> Result<Vec<OperationRecord>> {
    let mut operations = Vec::new();
    for record in records {
        if matches_dispatch(requests, &record)?
            && !operations.iter().any(|old: &OperationRecord| {
                old.request.operation_id == record.request.operation_id
            })
        {
            operations.push(record);
        }
    }
    Ok(operations)
}

fn status_records(status: &Value) -> Result<Vec<OperationRecord>> {
    let mut records: Vec<OperationRecord> = status
        .get("operations")
        .map(|value| serde_json::from_value(value.clone()))
        .transpose()?
        .unwrap_or_default();
    if let Some(operation) = status.get("operation") {
        let operation: OperationRecord = serde_json::from_value(operation.clone())?;
        if let Some(duplicate) = records
            .iter()
            .find(|record| record.request.operation_id == operation.request.operation_id)
        {
            ensure!(
                duplicate == &operation,
                "executor status has conflicting operation records"
            );
        } else {
            records.push(operation);
        }
    }
    Ok(records)
}

fn bound_request(
    store: &Store,
    context: &Context,
    proof: &SignedBinding,
) -> Result<Option<OperationRequest>> {
    let binding = &proof.binding;
    let mut proven = Vec::new();
    for request in bound_dispatches(context, proof)? {
        if !matches!(request.action["type"].as_str(), Some("open" | "send")) {
            continue;
        }
        let admitted =
            if let Some(operation) = store.operation(&request.network_id, &request.operation_id)? {
                matches_dispatch(std::slice::from_ref(&request), &operation)?
                    && operation.state != OperationState::Rejected
            } else {
                false
            };
        let observed = context.events.iter().any(|event|event.network_id == binding.network_id && event.scope_id == binding.conversation_id && event.author == binding.target_node && matches!(&event.body,EventBody::ExecutionObserved { operation_id,task_id:Some(task),target_node,session_id,status,.. } if operation_id == &request.operation_id && task == &binding.task_id && target_node == &binding.target_node && session_id == &binding.session_id && status != "rejected"));
        if admitted || observed {
            proven.push(request);
        }
    }
    let Some(revision) = proven.iter().map(|request| request.expected_revision).max() else {
        return Ok(None);
    };
    proven.retain(|request| request.expected_revision == revision);
    ensure!(
        proven.len() == 1,
        "task has competing execution requests; reconcile before controlling it"
    );
    Ok(proven.pop())
}

fn pending_requests(snapshot: &Value, native: Option<&str>, turn: Option<&str>) -> Vec<Value> {
    let (Some(native), Some(turn)) = (native, turn) else {
        return Vec::new();
    };
    snapshot["pending_requests"].as_array().into_iter().flatten()
        .filter(|request| request["params"]["threadId"].as_str() == Some(native))
        .filter(|request| request["params"]["turnId"].as_str() == Some(turn))
        .filter(|request| serde_json::from_value::<vibeface_protocol::RpcId>(request["id"].clone()).is_ok())
        .map(|request| {
            let method = request["method"].as_str().unwrap_or("request");
            let mut projected = json!({"request_id":request["id"],"kind":method,"text":request_text(method,&request["params"])});
            if matches!(method, "item/commandExecution/requestApproval" | "item/fileChange/requestApproval") {
                let decisions = request["params"]["availableDecisions"].as_array();
                let choices = [("批准一次", "accept"), ("拒绝", "decline"), ("取消本轮", "cancel")]
                    .into_iter()
                    .filter(|(_, decision)| decisions.is_none_or(|allowed| allowed.iter().any(|value| value.as_str() == Some(*decision))))
                    .map(|(label, decision)| json!({"label":label,"response":{"decision":decision}}))
                    .collect::<Vec<_>>();
                projected["choices"] = json!(choices);
            }
            if method == "item/tool/requestUserInput"
                && let Some(questions) = request["params"]["questions"].as_array()
            {
                projected["questions"] = json!(questions.iter().filter(|question|question["id"].is_string() && question["question"].is_string() && question["header"].is_string()).map(|question|json!({"id":question["id"],"header":question["header"],"question":question["question"],"is_secret":question["isSecret"].as_bool().unwrap_or(false),"options":question["options"].as_array().into_iter().flatten().filter(|option|option["label"].is_string()).map(|option|json!({"label":option["label"],"description":option["description"].as_str()})).collect::<Vec<_>>()})).collect::<Vec<_>>());
            }
            projected
        }).collect()
}

fn executor_capabilities(executor: &str, reachable: bool) -> Vec<&'static str> {
    serde_json::from_value::<ExecutorKind>(json!(executor))
        .map(|kind| {
            kind.capabilities()
                .iter()
                .copied()
                .filter(|capability| reachable || matches!(*capability, "open" | "send"))
                .collect()
        })
        .unwrap_or_default()
}

fn request_text(method: &str, params: &Value) -> String {
    let mut lines = vec![match method {
        "item/commandExecution/requestApproval" => "执行器请求运行命令。".to_owned(),
        "item/fileChange/requestApproval" => "执行器请求修改文件。".to_owned(),
        "item/tool/requestUserInput" => "执行器需要你补充信息。".to_owned(),
        _ => return "执行器发来暂不支持的请求，请到执行设备处理。".into(),
    }];
    if let Some(reason) = params["reason"]
        .as_str()
        .filter(|reason| !reason.trim().is_empty())
    {
        lines.push(reason.into());
    }
    if method == "item/commandExecution/requestApproval" {
        if let Some(command) = params["command"].as_str() {
            lines.push(format!("命令：{command}"));
        } else if let Some(command) = params["command"].as_array() {
            lines.push(format!(
                "命令：{}",
                command
                    .iter()
                    .filter_map(Value::as_str)
                    .collect::<Vec<_>>()
                    .join(" ")
            ));
        }
        if let Some(cwd) = params["cwd"].as_str() {
            lines.push(format!("目录：{cwd}"));
        }
    }
    if method == "item/fileChange/requestApproval" {
        if let Some(root) = params["grantRoot"].as_str() {
            lines.push(format!("允许修改的目录：{root}"));
        }
        if let Some(changes) = params["changes"].as_object() {
            lines.push(format!(
                "文件：{}",
                changes.keys().cloned().collect::<Vec<_>>().join("、")
            ));
        }
    }
    lines.join("\n")
}

fn artifact_page(hash: &str, bytes: &[u8], offset: u64, limit: u64) -> Result<Value> {
    ensure!(
        (4..=ARTIFACT_PAGE as u64).contains(&limit),
        "artifact limit must be between 4 and 65536 bytes"
    );
    let text = std::str::from_utf8(bytes).context("artifact is not UTF-8 text")?;
    let start = usize::try_from(offset).context("artifact offset is out of range")?;
    ensure!(
        start <= bytes.len() && text.is_char_boundary(start),
        "artifact offset is not a valid UTF-8 boundary"
    );
    let mut end = start.saturating_add(limit as usize).min(bytes.len());
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    let next = (end < bytes.len()).then_some(end);
    Ok(
        json!({"blob_hash":hash,"text":&text[start..end],"offset":offset,"next_offset":next,"size":bytes.len()}),
    )
}

impl Runtime {
    pub(super) async fn conversation_snapshot(
        &self,
        access: &Access,
        input: &Value,
    ) -> Result<Value> {
        let scope = field(input, "conversation_id")?;
        self.require_scope(access, scope).await?;
        let network = &access.network_id;
        let local = self.identity.node_id()?;
        let binding_error = self
            .cache_context_bindings(network, scope)
            .await
            .err()
            .map(|error| format!("{error:#}"));
        let jobs = self.scheduler_snapshot_jobs(network, scope).await?;
        let engine = self.engine.snapshot().await;
        let store = self.store.lock().await;
        let context = store.context(network, scope, &heads(input)?)?;
        let mut targets = jobs
            .iter()
            .filter_map(|job| job["target_node"].as_str().map(String::from))
            .collect::<BTreeSet<_>>();
        let mut messages = Vec::new();
        for message in store.messages(network, scope)? {
            let mut value = json!(message);
            let event = store
                .event(network, &message.event_hash)?
                .context("message event is missing")?;
            let event = store.verify_event(network, &event)?;
            value["blobs"] = json!(event.blobs);
            if let EventBody::MessagePosted {
                scheduler_node: Some(target),
                ..
            } = event.body
            {
                targets.insert(target);
            }
            messages.push(value);
        }
        let mut tasks = Vec::new();
        let mut sessions = Vec::new();
        let mut operations = Vec::new();
        let local_operations = store.operations(network)?;
        for task in store.tasks(network, scope)? {
            let mut value = json!(task);
            match store.followup_policy(network, scope, &task.task_id) {
                Ok(policy) => {
                    value["followup_mode"] = json!(policy.mode);
                    value["followup_version"] = json!(policy.version);
                    if !policy.target_node.is_empty() {
                        value["followup_scheduler_node"] = json!(policy.target_node);
                    }
                }
                Err(error) => {
                    value["followup_mode"] = json!("off");
                    value["followup_version"] = json!(0);
                    value["followup_error"] = json!(format!("{error:#}"));
                }
            }
            let blobs = context.events.iter().filter(|event| event.scope_id == scope && matches!(&event.body, EventBody::ExecutionObserved { task_id: Some(id), .. } if id == &task.task_id)).flat_map(|event| event.blobs.iter().cloned()).collect::<BTreeSet<_>>();
            value["blobs"] = json!(blobs);
            let creation = match store.task_creation(network, scope, &task.task_id) {
                Ok(creation) if binding_error.is_none() => creation,
                _ => {
                    value["binding_error"] = json!(
                        binding_error
                            .as_deref()
                            .unwrap_or("task history is incomplete or conflicting")
                    );
                    tasks.push(value);
                    continue;
                }
            };
            if let Some(proof) = store.task_binding(network, &creation.hash)? {
                let binding = &proof.binding;
                if let Err(error) =
                    store.authorize_node(network, &binding.target_node, Some(NodeRole::Executor))
                {
                    value["binding_error"] = json!(format!("{error:#}"));
                    tasks.push(value);
                    continue;
                }
                targets.insert(binding.target_node.clone());
                let request = match bound_request(&store, &context, &proof) {
                    Ok(request) => request,
                    Err(error) => {
                        value["binding_error"] = json!(format!("{error:#}"));
                        tasks.push(value);
                        continue;
                    }
                };
                let requests = bound_dispatches(&context, &proof)?;
                let cached = store.local_value(
                    network,
                    &session_cache_key(&binding.target_node, &binding.session_id),
                )?;
                let local_target = binding.target_node == local;
                let cached_records = cached
                    .as_ref()
                    .filter(|_| !local_target)
                    .map(status_records)
                    .transpose()?
                    .unwrap_or_default();
                let records = bound_operations(
                    &requests,
                    local_operations.iter().cloned().chain(cached_records),
                )?;
                let local_session =
                    store.session(network, &binding.target_node, &binding.session_id)?;
                let session = local_session.or_else(|| {
                    cached.as_ref().and_then(|value| {
                        serde_json::from_value::<ExecutionSession>(value["session"].clone())
                            .ok()
                            .filter(|session| {
                                session.network_id == *network
                                    && session.target_node == binding.target_node
                                    && session.session_id == binding.session_id
                            })
                    })
                });
                let native = session
                    .as_ref()
                    .and_then(|session| session.native_session_id.as_deref());
                let actual = if local_target {
                    engine.clone()
                } else {
                    cached
                        .as_ref()
                        .map(|value| value["snapshot"].clone())
                        .unwrap_or(Value::Null)
                };
                let reachable = local_target && actual["connection"]["state"] == "connected";
                let executor = session
                    .as_ref()
                    .map(|session| session.executor.as_str())
                    .or_else(|| binding.open_spec["executor"].as_str())
                    .unwrap_or("");
                let operation = request.as_ref().and_then(|request| {
                    records
                        .iter()
                        .find(|record| record.request.operation_id == request.operation_id)
                });
                let turn = if local_target {
                    request
                        .as_ref()
                        .map(|request| {
                            store.local_value(network, &format!("turn:{}", request.operation_id))
                        })
                        .transpose()?
                        .flatten()
                } else if operation.is_some() {
                    cached
                        .as_ref()
                        .filter(|value| {
                            request.as_ref().is_some_and(|request| {
                                value["operation"]["request"]["operation_id"].as_str()
                                    == Some(request.operation_id.as_str())
                            })
                        })
                        .map(|value| value["turn"].clone())
                } else {
                    None
                };
                let turn_id = turn.as_ref().and_then(|value| value["turn"]["id"].as_str());
                let mut projected = json!({"target_node":binding.target_node,"session_id":binding.session_id,"revision":session.as_ref().map(|session|session.revision).unwrap_or(0),"state":session.as_ref().map(|session|session.state.as_str()).unwrap_or("unknown"),"capabilities":executor_capabilities(executor, reachable || (!local_target && cached.as_ref().is_some_and(|value| value["reachable"] == true))),"pending_requests":pending_requests(&actual,native,turn_id),"freshness":if local_target {"local_ledger"} else if cached.is_some() {"cached"} else {"unknown"}});
                if let Some(cached) = &cached {
                    projected["checked_at_ms"] = cached["checked_at_ms"].clone();
                }
                if let Some(request) = request {
                    projected["operation_id"] = json!(request.operation_id);
                }
                if let Some(session) = session {
                    sessions.push(json!(session));
                }
                operations.extend(records.into_iter().map(|operation| json!(operation)));
                value["binding"] = projected;
            }
            tasks.push(value);
        }
        targets.retain(|target| store.authorize_node(network, target, None).is_ok());
        let frozen = targets.iter().any(|target| {
            store
                .local_value(
                    network,
                    &crate::scheduling::scheduler_key("session", scope, target, ""),
                )
                .ok()
                .flatten()
                .is_some_and(|value| {
                    value["unresolved"]
                        .as_array()
                        .is_some_and(|unresolved| !unresolved.is_empty())
                })
        }) || jobs.iter().any(|job| job["write_frozen"] == true);
        let manager = self.config.manager.as_ref();
        let capabilities = manager.and_then(|manager| match manager.provider.as_str() {
            "cursor_sdk" => Some(crate::manager::ManagerKind::CursorSdk.capabilities()),
            "codex_sdk" => Some(crate::manager::ManagerKind::CodexSdk.capabilities()),
            _ => None,
        });
        let remote_manager = jobs
            .iter()
            .find(|job| {
                matches!(
                    job["state"].as_str(),
                    Some("claimed" | "running" | "cancelling")
                ) && job["manager"].is_object()
            })
            .or_else(|| jobs.iter().rev().find(|job| job["manager"].is_object()))
            .map(|job| job["manager"].clone());
        let metadata = remote_manager.unwrap_or_else(||json!({"provider":manager.map(|manager|&manager.provider),"model":manager.map(|manager|&manager.model),"capabilities":capabilities}));
        Ok(
            json!({"conversation":store.conversations(network)?.into_iter().find(|conversation|conversation.conversation_id==scope),"messages":messages,"tasks":tasks,"heads":context.heads,"missing":context.missing,"sessions":sessions,"operations":operations,"facts_freshness":"local_known_history","scheduler":{"jobs":jobs,"provider":metadata["provider"],"model":metadata["model"],"capabilities":metadata["capabilities"],"write_frozen":frozen},"observation_targets":targets}),
        )
    }

    pub(super) async fn read_artifact(&self, access: &Access, input: &Value) -> Result<Value> {
        ensure!(access.human, "artifact access requires the user");
        let scope = field(input, "conversation_id")?;
        self.require_scope(access, scope).await?;
        let hash = field(input, "blob_hash")?;
        ensure!(
            hash.len() == 64 && hash.bytes().all(|byte| byte.is_ascii_hexdigit()),
            "invalid artifact hash"
        );
        let store = self.store.lock().await;
        let context = store.context(&access.network_id, scope, &[])?;
        let reference = context
            .events
            .iter()
            .filter(|event| event.scope_id == scope)
            .flat_map(|event| &event.blobs)
            .find(|blob| blob.hash == hash)
            .context("artifact is not referenced by this conversation")?;
        let bytes = store
            .get_blob(&access.network_id, hash)?
            .context("artifact content is pending synchronization")?;
        ensure!(
            reference.size == bytes.len() as u64 && crypto::digest(&bytes) == hash,
            "artifact content does not match its signed reference"
        );
        artifact_page(
            hash,
            &bytes,
            input["offset"].as_u64().unwrap_or(0),
            input["limit"].as_u64().unwrap_or(ARTIFACT_PAGE as u64),
        )
    }

    async fn chat_binding(
        &self,
        network: &str,
        scope: &str,
        task: &str,
    ) -> Result<(SignedBinding, Context)> {
        self.cache_context_bindings(network, scope).await?;
        let store = self.store.lock().await;
        let creation = store.task_creation(network, scope, task)?;
        let proof = store
            .task_binding(network, &creation.hash)?
            .context("task has no confirmed execution binding")?;
        store.authorize_node(
            network,
            &proof.binding.target_node,
            Some(NodeRole::Executor),
        )?;
        let context = store.context(network, scope, &[])?;
        ensure!(
            context.missing.is_complete(),
            "task history is incomplete or conflicting"
        );
        Ok((proof, context))
    }

    pub(super) async fn chat_task_status(
        &self,
        network: &str,
        scope: &str,
        task: &str,
    ) -> Result<Value> {
        let (proof, context) = self.chat_binding(network, scope, task).await?;
        ensure!(
            proof.binding.target_node == self.identity.node_id()?,
            "task is bound to another executor"
        );
        let (session, operation, operations, turn) = {
            let store = self.store.lock().await;
            let requests = bound_dispatches(&context, &proof)?;
            let request = bound_request(&store, &context, &proof)?
                .context("task has no admitted execution")?;
            let operations = bound_operations(&requests, store.operations(network)?)?;
            (
                store
                    .session(network, &request.target_node, &request.session_id)?
                    .context("execution session not found")?,
                store
                    .operation(network, &request.operation_id)?
                    .context("original operation not found")?,
                operations,
                store.local_value(network, &format!("turn:{}", request.operation_id))?,
            )
        };
        let read_error = if session.executor == "codex" && session.native_session_id.is_some() {
            let native = ExecutorSession {
                executor: ExecutorKind::Codex,
                cwd: session.cwd.clone().into(),
                native_session_id: session.native_session_id.clone(),
            };
            match tokio::time::timeout(
                Duration::from_secs(5),
                self.engine
                    .execute(&native, EngineAction::Read {}, |_| async {
                        bail!("status must not create a native session")
                    }),
            )
            .await
            {
                Ok(Ok(_)) => None,
                Ok(Err(error)) => Some(error.to_string()),
                Err(_) => Some("executor status read timed out".into()),
            }
        } else {
            None
        };
        let snapshot = self.engine.snapshot().await;
        let native = session.native_session_id.as_deref();
        let mut filtered = json!({"connection":snapshot["connection"],"threads":{},"pending_requests":snapshot["pending_requests"].as_array().into_iter().flatten().filter(|value| native.is_some_and(|native|value["params"]["threadId"].as_str()==Some(native))).cloned().collect::<Vec<_>>()});
        if let Some(native) = native {
            filtered["threads"][native] = snapshot["threads"][native].clone();
        }
        Ok(
            json!({"session":session,"operation":operation,"operations":operations,"turn":turn,"snapshot":filtered,"reachable":read_error.is_none() && snapshot["connection"]["state"]=="connected","read_error":read_error,"checked_at_ms":now_ms()}),
        )
    }

    pub(super) async fn task_control(
        self: &Arc<Self>,
        access: &Access,
        input: &Value,
    ) -> Result<Value> {
        ensure!(access.human, "task controls require the user");
        let scope = field(input, "conversation_id")?;
        self.require_scope(access, scope).await?;
        let task = field(input, "task_id")?;
        let action = field(input, "action")?;
        ensure!(
            matches!(action, "status" | "steer" | "interrupt" | "respond_request"),
            "unknown task control"
        );
        let (proof, context) = self.chat_binding(&access.network_id, scope, task).await?;
        for (key, expected) in [
            ("network_id", access.network_id.as_str()),
            ("target_node", proof.binding.target_node.as_str()),
            ("session_id", proof.binding.session_id.as_str()),
        ] {
            if let Some(value) = input.get(key) {
                ensure!(
                    value.as_str() == Some(expected),
                    "task control {key} does not match its binding"
                );
            }
        }
        let intent = json!({"conversation_id":scope,"task_id":task,"action":action,"expected_operation_id":input["expected_operation_id"],"expected_revision":input["expected_revision"],"text":input["text"],"request_id":input["request_id"],"response":input["response"]});
        let operation_id = if action == "status" {
            None
        } else {
            Some(id(input, "operation_id")?)
        };
        let prepared = prepared_request(
            &*self.store.lock().await,
            &access.network_id,
            operation_id.as_deref(),
            &intent,
        )?;
        if let Some(request) = prepared {
            return self.dispatch(access.clone(), request).await;
        }
        let status = if proof.binding.target_node == self.identity.node_id()? {
            self.chat_task_status(&access.network_id, scope, task)
                .await?
        } else {
            self.read_peer(
                &access.network_id,
                &proof.binding.target_node,
                json!({"type":"chat_task_status","conversation_id":scope,"task_id":task}),
            )
            .await?
        };
        let prepared = prepared_request(
            &*self.store.lock().await,
            &access.network_id,
            operation_id.as_deref(),
            &intent,
        )?;
        if let Some(request) = prepared {
            return self.dispatch(access.clone(), request).await;
        }
        let session: ExecutionSession = serde_json::from_value(status["session"].clone())?;
        let operation: OperationRecord = serde_json::from_value(status["operation"].clone())?;
        let requests = bound_dispatches(&context, &proof)?;
        let original = requests
            .iter()
            .find(|request| request.operation_id == operation.request.operation_id)
            .context("executor status has no dispatched task operation")?;
        ensure!(
            session.network_id == access.network_id
                && session.target_node == proof.binding.target_node
                && session.session_id == proof.binding.session_id
                && operation.state != OperationState::Rejected
                && matches!(original.action["type"].as_str(), Some("open" | "send"))
                && same_business_request(&operation.request, original),
            "executor status does not match the original task binding"
        );
        for record in status_records(&status)? {
            ensure!(
                matches_dispatch(&requests, &record)?,
                "executor status contains an operation outside this task binding"
            );
        }
        self.store.lock().await.local_set(
            &access.network_id,
            &session_cache_key(&session.target_node, &session.session_id),
            &status,
        )?;
        if action == "status" {
            return Ok(status);
        }
        ensure!(
            field(input, "expected_operation_id")? == original.operation_id,
            "task operation changed; refresh its status"
        );
        let revision = input["expected_revision"]
            .as_u64()
            .context("expected_revision is required")?;
        ensure!(
            revision == session.revision,
            "execution revision changed; refresh its status"
        );
        ensure!(
            session.executor == "codex" && status["reachable"] == true,
            "this executor does not provide live task controls"
        );
        let native_action = resolve_control(action, input, &status, &session)?;
        self.engine_action(&native_action)?.validate()?;
        let mut request = OperationRequest {
            operation_id: operation_id.context("operation_id required")?,
            network_id: access.network_id.clone(),
            conversation_id: scope.into(),
            task_id: Some(task.into()),
            target_node: session.target_node.clone(),
            session_id: session.session_id.clone(),
            expected_revision: revision,
            basis_heads: self.store.lock().await.heads(&access.network_id, scope)?,
            action: native_action,
            resources: vec![],
            binding: Some(proof),
            manager_authority: None,
        };
        {
            let mut store = self.store.lock().await;
            if let Some(old) =
                store.local_value(&access.network_id, &control_key(&request.operation_id))?
            {
                ensure!(
                    old["intent"] == intent,
                    "operation ID already prepared with different control data"
                );
                request = serde_json::from_value(old["request"].clone())?;
            } else {
                store.local_set(
                    &access.network_id,
                    &control_key(&request.operation_id),
                    &json!({"intent":intent,"request":request}),
                )?;
            }
        }
        self.dispatch(access.clone(), request).await
    }
}

fn resolve_control(
    action: &str,
    input: &Value,
    status: &Value,
    session: &ExecutionSession,
) -> Result<Value> {
    let native = session
        .native_session_id
        .as_deref()
        .context("execution has no native session")?;
    let turn = status["turn"]["turn"]["id"].as_str().context(
        "original operation has no confirmed native turn; refresh after synchronization",
    )?;
    let current_turn = &status["snapshot"]["threads"][native]["last_turn"];
    ensure!(
        current_turn["id"].as_str() == Some(turn)
            && matches!(
                current_turn["status"].as_str(),
                Some("inProgress" | "in_progress" | "running")
            ),
        "the original native turn is no longer active"
    );
    if action == "respond_request" {
        let request_id = input.get("request_id").context("request_id is required")?;
        serde_json::from_value::<vibeface_protocol::RpcId>(request_id.clone())?;
        let current = status["snapshot"]["pending_requests"]
            .as_array()
            .into_iter()
            .flatten()
            .find(|request| {
                request.get("id") == Some(request_id)
                    && request["params"]["threadId"].as_str() == Some(native)
            })
            .context("approval request expired or belongs to another task")?;
        ensure!(
            input.get("response").is_some_and(Value::is_object),
            "a human response object is required"
        );
        ensure!(
            current["params"]["turnId"].as_str() == Some(turn),
            "approval does not belong to the original operation"
        );
        return Ok(
            json!({"type":"respond_request","request_id":request_id,"response":input["response"]}),
        );
    }
    match action {
        "steer" => Ok(json!({"type":"steer","turn_id":turn,"text":field(input,"text")?})),
        "interrupt" => Ok(json!({"type":"interrupt","turn_id":turn})),
        _ => bail!("invalid mutable task control"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prepared_control_reuses_frozen_body_and_is_network_scoped() -> Result<()> {
        let mut store = Store::open(":memory:")?;
        let intent =
            json!({"action":"interrupt","expected_operation_id":"send","expected_revision":1});
        let request: OperationRequest = serde_json::from_value(
            json!({"operation_id":"control","network_id":"network","conversation_id":"chat","task_id":"task","target_node":"executor","session_id":"session","expected_revision":1,"basis_heads":["old-head"],"action":{"type":"interrupt","turn_id":"turn"},"resources":[]}),
        )?;
        store.local_set(
            "network",
            &control_key("control"),
            &json!({"intent":intent,"request":request}),
        )?;
        assert_eq!(
            prepared_request(&store, "network", Some("control"), &intent)?,
            Some(request.clone())
        );
        assert!(prepared_request(&store, "other-network", Some("control"), &intent)?.is_none());
        assert!(
            prepared_request(
                &store,
                "network",
                Some("control"),
                &json!({"action":"steer"})
            )
            .is_err()
        );
        let mut admitted = request.clone();
        admitted.resources = vec!["/canonical/workspace".into()];
        assert!(same_business_request(&admitted, &request));
        admitted.action = json!({"type":"interrupt","turn_id":"other"});
        assert!(!same_business_request(&admitted, &request));
        Ok(())
    }

    #[test]
    fn competing_dispatches_require_the_original_executors_proof() -> Result<()> {
        let store = Store::open(":memory:")?;
        let proof: SignedBinding = serde_json::from_value(
            json!({"signature":"signature","binding":{"network_id":"network","conversation_id":"chat","task_ref":"creation","task_id":"task","authority_node":"scheduler","generation":1,"target_node":"executor","session_id":"session","open_spec":{"type":"open","executor":"codex","cwd":"/tmp"},"previous_binding_hash":null,"previous_seal":null}}),
        )?;
        let event = |author: &str, body: EventBody| Event {
            version: 1,
            network_id: "network".into(),
            scope_id: "chat".into(),
            author: author.into(),
            sequence: 1,
            previous: None,
            parents: vec![],
            created_at_ms: 0,
            body,
            blobs: vec![],
        };
        let mut context = Context {
            network_id: "network".into(),
            scope_id: "chat".into(),
            heads: vec![],
            events: vec![],
            missing: MissingDependencies::default(),
        };
        for operation in ["winner", "rejected"] {
            let request: OperationRequest = serde_json::from_value(
                json!({"operation_id":operation,"network_id":"network","conversation_id":"chat","task_id":"task","target_node":"executor","session_id":"session","expected_revision":1,"basis_heads":[],"action":{"type":"send","text":"work"},"resources":[],"binding":proof}),
            )?;
            context.events.push(event(
                "scheduler",
                EventBody::DispatchRecorded {
                    operation_id: operation.into(),
                    task_id: Some("task".into()),
                    target_node: "executor".into(),
                    session_id: "session".into(),
                    action: json!(request),
                },
            ));
        }
        assert!(bound_request(&store, &context, &proof)?.is_none());
        let observed = EventBody::ExecutionObserved {
            operation_id: "winner".into(),
            task_id: Some("task".into()),
            target_node: "executor".into(),
            session_id: "session".into(),
            status: "running".into(),
            detail: json!({}),
        };
        context.events.push(event("other-node", observed.clone()));
        assert!(bound_request(&store, &context, &proof)?.is_none());
        context.events.push(event("executor", observed));
        assert_eq!(
            bound_request(&store, &context, &proof)?
                .expect("proven winner")
                .operation_id,
            "winner"
        );
        let mut unadmitted = bound_dispatches(&context, &proof)?[0].clone();
        unadmitted.operation_id = "newer-unadmitted-send".into();
        unadmitted.expected_revision = 9;
        context.events.push(event(
            "scheduler",
            EventBody::DispatchRecorded {
                operation_id: unadmitted.operation_id.clone(),
                task_id: unadmitted.task_id.clone(),
                target_node: unadmitted.target_node.clone(),
                session_id: unadmitted.session_id.clone(),
                action: json!(unadmitted),
            },
        ));
        assert_eq!(
            bound_request(&store, &context, &proof)?
                .unwrap()
                .operation_id,
            "winner"
        );
        context.events.push(event(
            "executor",
            EventBody::ExecutionObserved {
                operation_id: "rejected".into(),
                task_id: Some("task".into()),
                target_node: "executor".into(),
                session_id: "session".into(),
                status: "running".into(),
                detail: json!({}),
            },
        ));
        assert!(bound_request(&store, &context, &proof).is_err());
        Ok(())
    }

    #[test]
    fn unknown_controls_survive_status_cache_without_replacing_the_primary_operation() -> Result<()>
    {
        let proof: SignedBinding = serde_json::from_value(
            json!({"signature":"signature","binding":{"network_id":"network","conversation_id":"chat","task_ref":"creation","task_id":"task","authority_node":"scheduler","generation":1,"target_node":"executor","session_id":"session","open_spec":{"type":"open","executor":"codex","cwd":"/tmp"},"previous_binding_hash":null,"previous_seal":null}}),
        )?;
        let primary: OperationRequest = serde_json::from_value(
            json!({"operation_id":"send","network_id":"network","conversation_id":"chat","task_id":"task","target_node":"executor","session_id":"session","expected_revision":1,"basis_heads":[],"action":{"type":"send","text":"work"},"resources":[],"binding":proof}),
        )?;
        let mut control = primary.clone();
        control.operation_id = "unknown-control".into();
        control.expected_revision = 2;
        control.action = json!({"type":"interrupt","turn_id":"turn"});
        let dispatch = |request: &OperationRequest| Event {
            version: 1,
            network_id: request.network_id.clone(),
            scope_id: request.conversation_id.clone(),
            author: "scheduler".into(),
            sequence: 1,
            previous: None,
            parents: vec![],
            created_at_ms: 0,
            body: EventBody::DispatchRecorded {
                operation_id: request.operation_id.clone(),
                task_id: request.task_id.clone(),
                target_node: request.target_node.clone(),
                session_id: request.session_id.clone(),
                action: json!(request),
            },
            blobs: vec![],
        };
        let mut context = Context {
            network_id: "network".into(),
            scope_id: "chat".into(),
            heads: vec![],
            events: vec![dispatch(&primary), dispatch(&control)],
            missing: MissingDependencies::default(),
        };
        context.events.push(Event {
            author: "executor".into(),
            body: EventBody::ExecutionObserved {
                operation_id: primary.operation_id.clone(),
                task_id: primary.task_id.clone(),
                target_node: primary.target_node.clone(),
                session_id: primary.session_id.clone(),
                status: "running".into(),
                detail: json!({}),
            },
            ..dispatch(&primary)
        });
        let primary_record = OperationRecord {
            request: primary.clone(),
            state: OperationState::Running,
            result: Value::Null,
        };
        let control_record = OperationRecord {
            request: control.clone(),
            state: OperationState::Unknown,
            result: json!({"error":"connection lost"}),
        };
        let mut records = vec![primary_record.clone(), control_record.clone()];
        for field in [
            "network_id",
            "conversation_id",
            "task_id",
            "target_node",
            "session_id",
            "binding",
        ] {
            let mut unrelated = serde_json::to_value(&control)?;
            unrelated["operation_id"] = json!(format!("other-{field}"));
            if field == "binding" {
                unrelated["binding"]["binding"]["generation"] = json!(2);
            } else {
                unrelated[field] = json!("other");
            }
            let unrelated: OperationRequest = serde_json::from_value(unrelated)?;
            context.events.push(dispatch(&unrelated));
            records.push(OperationRecord {
                request: unrelated,
                state: OperationState::Unknown,
                result: Value::Null,
            });
        }
        let requests = bound_dispatches(&context, &proof)?;
        let exposed = bound_operations(&requests, records)?;
        assert_eq!(
            exposed,
            vec![primary_record.clone(), control_record.clone()]
        );
        let cached = json!({"operation":primary_record,"operations":exposed});
        assert_eq!(
            bound_operations(&requests, status_records(&cached)?)?,
            exposed
        );
        assert_eq!(
            status_records(&json!({"operation":primary_record}))?,
            vec![primary_record.clone()]
        );
        assert_eq!(
            bound_request(&Store::open(":memory:")?, &context, &proof)?.unwrap(),
            primary
        );
        let mut wrong_body = control_record.clone();
        wrong_body.request.action = json!({"type":"interrupt","turn_id":"other-turn"});
        assert!(matches_dispatch(&requests, &wrong_body).is_err());
        let mut foreign = control_record;
        foreign.request.operation_id = "not-dispatched".into();
        assert!(!matches_dispatch(&requests, &foreign)?);
        context.events.push(dispatch(&wrong_body.request));
        assert!(bound_dispatches(&context, &proof).is_err());
        Ok(())
    }

    #[test]
    fn artifact_pages_preserve_utf8_and_reject_split_offsets() -> Result<()> {
        let bytes = "a中文🙂z".as_bytes();
        let first = artifact_page("hash", bytes, 0, 4)?;
        assert_eq!(first["text"], "a中");
        assert_eq!(first["next_offset"], 4);
        let second = artifact_page("hash", bytes, 4, 4)?;
        assert_eq!(second["text"], "文");
        assert_eq!(second["next_offset"], 7);
        assert_eq!(artifact_page("hash", bytes, 7, 4)?["text"], "🙂");
        assert!(artifact_page("hash", bytes, 2, 4).is_err());
        assert!(artifact_page("hash", bytes, 0, 3).is_err());
        assert!(artifact_page("hash", &[255], 0, 4).is_err());
        Ok(())
    }

    #[test]
    fn control_resolves_only_the_original_active_turn_and_current_request() -> Result<()> {
        let session = ExecutionSession {
            network_id: "network".into(),
            target_node: "executor".into(),
            session_id: "session".into(),
            revision: 1,
            state: "running".into(),
            executor: "codex".into(),
            cwd: "/tmp".into(),
            native_session_id: Some("native".into()),
        };
        let mut status = json!({"turn":{"turn":{"id":"original"}},"snapshot":{"threads":{"native":{"last_turn":{"id":"original","status":"inProgress"}}},"pending_requests":[{"id":7,"params":{"threadId":"native","turnId":"original"}}]}});
        assert_eq!(
            resolve_control("interrupt", &json!({}), &status, &session)?["turn_id"],
            "original"
        );
        status["snapshot"]["threads"]["native"]["last_turn"]["id"] = json!("other");
        assert!(resolve_control("interrupt", &json!({}), &status, &session).is_err());
        status["snapshot"]["threads"]["native"]["last_turn"]["id"] = json!("original");
        assert!(
            resolve_control(
                "respond_request",
                &json!({"request_id":8,"response":{"decision":"accept"}}),
                &status,
                &session
            )
            .is_err()
        );
        assert_eq!(
            resolve_control(
                "respond_request",
                &json!({"request_id":7,"response":{"decision":"decline"}}),
                &status,
                &session
            )?["request_id"],
            7
        );
        status["snapshot"]["pending_requests"][0]["params"]["turnId"] = json!("other");
        assert!(
            resolve_control(
                "respond_request",
                &json!({"request_id":7,"response":{"decision":"accept"}}),
                &status,
                &session
            )
            .is_err()
        );
        assert!(!executor_capabilities("cursor_agent", true).contains(&"interrupt"));
        assert!(!executor_capabilities("claude_code", true).contains(&"respond_request"));
        status["snapshot"]["pending_requests"][0]["params"]
            .as_object_mut()
            .unwrap()
            .remove("turnId");
        assert!(
            resolve_control(
                "respond_request",
                &json!({"request_id":7,"response":{"decision":"accept"}}),
                &status,
                &session
            )
            .is_err()
        );
        let snapshot = json!({"pending_requests":[{"id":1,"method":"item/tool/requestUserInput","params":{"questions":[]}}, {"id":2,"method":"item/commandExecution/requestApproval","params":{"threadId":"other","turnId":"original"}}]});
        assert!(pending_requests(&snapshot, None, Some("original")).is_empty());
        assert!(pending_requests(&snapshot, Some("native"), Some("original")).is_empty());
        let projected = pending_requests(
            &json!({"pending_requests":[{"id":9,"method":"item/commandExecution/requestApproval","params":{"threadId":"native","turnId":"original","command":"cargo check","cwd":"/workspace","reason":"核验修改"}}]}),
            Some("native"),
            Some("original"),
        );
        assert!(
            projected[0]["text"]
                .as_str()
                .unwrap()
                .contains("cargo check")
        );
        assert!(!projected[0]["text"].as_str().unwrap().contains("threadId"));
        assert!(!projected[0]["text"].as_str().unwrap().contains("turnId"));
        Ok(())
    }
}
