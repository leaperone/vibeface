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

fn bound_request(context: &Context, proof: &SignedBinding) -> Result<Option<OperationRequest>> {
    let binding = &proof.binding;
    let mut requests = Vec::new();
    for event in &context.events {
        if event.scope_id != binding.conversation_id {
            continue;
        }
        let EventBody::DispatchRecorded {
            action,
            operation_id,
            ..
        } = &event.body
        else {
            continue;
        };
        let Ok(request) = serde_json::from_value::<OperationRequest>(action.clone()) else {
            continue;
        };
        if request.network_id == binding.network_id
            && request.conversation_id == binding.conversation_id
            && request.task_id.as_deref() == Some(binding.task_id.as_str())
            && request.target_node == binding.target_node
            && request.session_id == binding.session_id
            && request.operation_id == *operation_id
            && request.binding.as_ref() == Some(proof)
            && matches!(request.action["type"].as_str(), Some("open" | "send"))
        {
            requests.push(request);
        }
    }
    let Some(revision) = requests
        .iter()
        .map(|request| request.expected_revision)
        .max()
    else {
        return Ok(None);
    };
    let mut latest = requests
        .into_iter()
        .filter(|request| request.expected_revision == revision);
    let request = latest.next();
    ensure!(
        latest.next().is_none(),
        "task has competing execution requests; reconcile before controlling it"
    );
    Ok(request)
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
            let mut projected = json!({"request_id":request["id"],"kind":method,"text":serde_json::to_string_pretty(&request["params"]).unwrap_or_default()});
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
    if executor == "codex" && reachable {
        vec![
            "open",
            "send",
            "read",
            "steer",
            "interrupt",
            "respond_request",
        ]
    } else {
        vec!["open", "send"]
    }
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
                let request = match bound_request(&context, &proof) {
                    Ok(request) => request,
                    Err(error) => {
                        value["binding_error"] = json!(format!("{error:#}"));
                        tasks.push(value);
                        continue;
                    }
                };
                let cached = store.local_value(
                    network,
                    &session_cache_key(&binding.target_node, &binding.session_id),
                )?;
                let local_session =
                    store.session(network, &binding.target_node, &binding.session_id)?;
                let session = local_session.or_else(|| {
                    cached.as_ref().and_then(|value| {
                        serde_json::from_value::<ExecutionSession>(value["session"].clone()).ok()
                    })
                });
                let native = session
                    .as_ref()
                    .and_then(|session| session.native_session_id.as_deref());
                let local_target = binding.target_node == local;
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
                let operation = request
                    .as_ref()
                    .and_then(|request| {
                        store
                            .operation(network, &request.operation_id)
                            .ok()
                            .flatten()
                    })
                    .or_else(|| {
                        cached.as_ref().and_then(|value| {
                            serde_json::from_value::<OperationRecord>(value["operation"].clone())
                                .ok()
                                .filter(|operation| {
                                    request.as_ref().is_some_and(|request| {
                                        same_business_request(&operation.request, request)
                                    })
                                })
                        })
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
                    cached.as_ref().map(|value| value["turn"].clone())
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
                if let Some(operation) = operation {
                    operations.push(json!(operation));
                }
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
    ) -> Result<(SignedBinding, OperationRequest)> {
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
        let request =
            bound_request(&context, &proof)?.context("task has no dispatched execution")?;
        Ok((proof, request))
    }

    pub(super) async fn chat_task_status(
        &self,
        network: &str,
        scope: &str,
        task: &str,
    ) -> Result<Value> {
        let (proof, request) = self.chat_binding(network, scope, task).await?;
        ensure!(
            proof.binding.target_node == self.identity.node_id()?,
            "task is bound to another executor"
        );
        let (session, operation, turn) = {
            let store = self.store.lock().await;
            (
                store
                    .session(network, &request.target_node, &request.session_id)?
                    .context("execution session not found")?,
                store
                    .operation(network, &request.operation_id)?
                    .context("original operation not found")?,
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
            json!({"session":session,"operation":operation,"turn":turn,"snapshot":filtered,"reachable":read_error.is_none() && snapshot["connection"]["state"]=="connected","read_error":read_error,"checked_at_ms":now_ms()}),
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
        let (proof, original) = self.chat_binding(&access.network_id, scope, task).await?;
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
        ensure!(
            session.network_id == access.network_id
                && session.target_node == proof.binding.target_node
                && session.session_id == proof.binding.session_id
                && same_business_request(&operation.request, &original),
            "executor status does not match the original task binding"
        );
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
        Ok(())
    }
}
