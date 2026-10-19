use super::*;

const ARTIFACT_PAGE: usize = 65_536;

pub(super) fn pending_requests(
    snapshot: &Value,
    native: Option<&str>,
    turn: Option<&str>,
) -> Vec<Value> {
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

fn request_text(method: &str, params: &Value) -> String {
    let mut lines = vec![match method {
        "item/commandExecution/requestApproval" => "原生 Agent请求运行命令。".to_owned(),
        "item/fileChange/requestApproval" => "原生 Agent请求修改文件。".to_owned(),
        "item/tool/requestUserInput" => "原生 Agent需要你补充信息。".to_owned(),
        _ => return "原生 Agent发来暂不支持的请求，请到执行设备处理。".into(),
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

fn native_cache_key(scope: &str, target: &str) -> String {
    format!(
        "native-snapshot:v1:{}",
        crypto::digest(&serde_json::to_vec(&(scope, target)).expect("string tuple serializes"))
    )
}

fn remote_native_sessions(reply: &Value, scope: &str, target: &str) -> Result<Vec<Value>> {
    let sessions = reply["native_sessions"]
        .as_array()
        .context("native observation response is invalid")?;
    for work in sessions {
        ensure!(
            work["conversation_id"] == scope
                && work["target_node"] == target
                && work["work_id"].as_str().is_some_and(|id| !id.is_empty())
                && work["thread_id"].as_str().is_some_and(|id| !id.is_empty()),
            "native observation identity mismatch"
        );
    }
    Ok(sessions.clone())
}

fn mark_native_freshness(work: &mut Value, cached: bool) {
    work["freshness"] = json!(if cached { "cached" } else { "observed" });
    work["can_control"] = json!(
        !cached
            && work["can_control"].as_bool() == Some(true)
            && matches!(work["state"].as_str(), Some("running" | "waiting"))
    );
    if cached
        && !matches!(
            work["state"].as_str(),
            Some("completed" | "failed" | "interrupted")
        )
    {
        work["state"] = json!("unknown");
    }
}

fn remote_native_projection(
    store: &mut Store,
    network: &str,
    scope: &str,
    target: &str,
    reply: Result<Value>,
) -> Result<Vec<Value>> {
    let key = native_cache_key(scope, target);
    let (mut sessions, cached) = match reply.and_then(|reply| {
        let sessions = remote_native_sessions(&reply, scope, target)?;
        ensure!(
            sessions
                .iter()
                .all(|work| work["freshness"] == "observed" && work["can_control"].is_boolean()),
            "native observation control metadata is invalid"
        );
        Ok(sessions)
    }) {
        Ok(sessions) => {
            let snapshot =
                json!({"conversation_id":scope,"target_node":target,"native_sessions":sessions});
            if store.local_value(network, &key)?.as_ref() != Some(&snapshot) {
                store.local_set(network, &key, &snapshot)?;
            }
            (sessions, false)
        }
        Err(error) => {
            tracing::debug!(network,scope,target,%error,"native observation unavailable; preserving its last authenticated snapshot");
            let Some(snapshot) = store.local_value(network, &key)? else {
                return Ok(Vec::new());
            };
            ensure!(
                snapshot["conversation_id"] == scope && snapshot["target_node"] == target,
                "cached native observation identity mismatch"
            );
            (remote_native_sessions(&snapshot, scope, target)?, true)
        }
    };
    for work in &mut sessions {
        mark_native_freshness(work, cached);
    }
    Ok(sessions)
}

impl Runtime {
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
}

impl Runtime {
    pub(super) async fn agent_task_projections(
        &self,
        network: &str,
        scope: &str,
    ) -> Result<Vec<Value>> {
        let store = self.store.lock().await;
        let context = store.context(network, scope, &[])?;
        let mut tasks = Vec::new();
        for event in context
            .events
            .iter()
            .filter(|event| event.scope_id == scope)
        {
            let EventBody::AgentMessage { message } = &event.body else {
                continue;
            };
            if message.kind != AgentMessageKind::Task {
                continue;
            }
            let task_ref = crypto::digest(&serde_json::to_vec(event)?);
            let status = store.agent_task_status(network, scope, &task_ref)?;
            tasks.push(json!({"task_ref":task_ref,"source_node":event.author,"target_node":message.target_node,
                "origin_trigger":message.origin_trigger,"state":status["state"],"text":message.text,
                "detail":status["result"]["text"].as_str().unwrap_or("")}));
        }
        Ok(tasks)
    }

    pub(super) async fn native_session_projections(
        &self,
        network: &str,
        scope: &str,
    ) -> Result<Vec<Value>> {
        let local = self.identity.peer_id()?;
        let mut sessions = self.native_status(network, scope).await?["native_sessions"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        for work in &mut sessions {
            mark_native_freshness(work, false);
        }
        let targets = {
            let store = self.store.lock().await;
            store
                .context(network, scope, &[])?
                .events
                .into_iter()
                .filter_map(|event| match event.body {
                    EventBody::AgentMessage { message }
                        if message.kind == AgentMessageKind::Task =>
                    {
                        Some(message.target_node)
                    }
                    EventBody::MessagePosted { scheduler_node, .. } => scheduler_node,
                    _ => None,
                })
                .filter(|target| {
                    target != &local
                        && store
                            .authorize_node(network, target, Some(NodeRole::Scheduler))
                            .is_ok()
                })
                .collect::<BTreeSet<_>>()
        };
        for target in targets {
            let result = tokio::time::timeout(
                Duration::from_secs(3),
                self.read_peer(
                    network,
                    &target,
                    json!({"type":"native_status","conversation_id":scope}),
                ),
            )
            .await;
            let reply = result
                .context("native observation timed out")
                .and_then(|result| result);
            sessions.extend(remote_native_projection(
                &mut *self.store.lock().await,
                network,
                scope,
                &target,
                reply,
            )?);
        }
        Ok(sessions)
    }

    pub(super) async fn conversation_snapshot(
        self: &Arc<Self>,
        access: &Access,
        input: &Value,
    ) -> Result<Value> {
        let scope = field(input, "conversation_id")?;
        self.require_scope(access, scope).await?;
        let network = &access.network_id;
        let jobs = self.scheduler_snapshot_jobs(network, scope).await?;
        let agent_tasks = self.agent_task_projections(network, scope).await?;
        let native_sessions = self.native_session_projections(network, scope).await?;
        let store = self.store.lock().await;
        let context = store.context(network, scope, &heads(input)?)?;
        let mut targets = jobs
            .iter()
            .filter_map(|job| job["target_node"].as_str().map(String::from))
            .collect::<BTreeSet<_>>();
        for task in &agent_tasks {
            if let Some(target) = task["target_node"].as_str() {
                targets.insert(target.into());
            }
            if let Some(source) = task["source_node"].as_str() {
                targets.insert(source.into());
            }
        }
        let mut messages = Vec::new();
        for message in store.messages(network, scope)? {
            let mut projected = json!(message);
            let envelope = store
                .event(network, &message.event_hash)?
                .context("message event is missing")?;
            let event = store.verify_event(network, &envelope)?;
            projected["blobs"] = json!(event.blobs);
            messages.push(projected);
        }
        let mut tasks = Vec::new();
        for task in store.tasks(network, scope)? {
            let mut projected = json!(task);
            match store.followup_policy(network, scope, &task.task_id) {
                Ok(policy) => {
                    projected["followup_mode"] = json!(policy.mode);
                    projected["followup_version"] = json!(policy.version);
                    projected["followup_scheduler_node"] = json!(policy.target_node);
                }
                Err(error) => {
                    projected["followup_mode"] = json!("off");
                    projected["followup_version"] = json!(0);
                    projected["followup_error"] = json!(error.to_string());
                }
            }
            tasks.push(projected);
        }
        targets.retain(|target| store.authorize_node(network, target, None).is_ok());
        let frozen = jobs.iter().any(|job| job["write_frozen"] == true);
        let settings = self.settings_snapshot();
        let manager = settings.manager.as_ref();
        let remote_manager = jobs
            .iter()
            .rev()
            .find(|job| job["manager"].is_object())
            .map(|job| job["manager"].clone());
        let metadata = remote_manager.unwrap_or_else(||json!({"provider":manager.map(|manager|&manager.provider),
            "model":manager.map(|manager|&manager.model),"capabilities":manager.and_then(|manager| match manager.provider.as_str() {
                "codex_sdk"=>Some(crate::manager::ManagerKind::CodexSdk.capabilities()),
                "cursor_sdk"=>Some(crate::manager::ManagerKind::CursorSdk.capabilities()),_=>None})}));
        let current_heads = store.heads(network, scope)?;
        let needs_sync = !store
            .missing_dependencies(network, &current_heads)?
            .is_complete();
        let snapshot = json!({"bot":store.bot_for_conversation(network,scope)?,"conversation":store.conversations(network)?.into_iter().find(|conversation|conversation.conversation_id==scope),
            "messages":messages,"tasks":tasks,"agent_tasks":agent_tasks,"native_sessions":native_sessions,"heads":context.heads,"missing":context.missing,
            "facts_freshness":"local_known_history","scheduler":{"jobs":jobs,"provider":metadata["provider"],"model":metadata["model"],"capabilities":metadata["capabilities"],"write_frozen":frozen},"observation_targets":targets});
        drop(store);
        if needs_sync {
            let _ = self.request_scope_sync(network, scope, current_heads).await;
        }
        Ok(snapshot)
    }

    pub(super) async fn cancel_task_rpc(
        self: &Arc<Self>,
        access: &Access,
        input: &Value,
    ) -> Result<Value> {
        ensure!(
            access.human,
            "only the user cancels a task through this endpoint"
        );
        let scope = field(input, "conversation_id")?;
        let task_ref = field(input, "task_ref")?;
        self.require_scope(access, scope).await?;
        let status =
            self.store
                .lock()
                .await
                .agent_task_status(&access.network_id, scope, task_ref)?;
        self.cancel_known_task(&access.network_id, scope, task_ref, &status)
            .await
    }

    pub(super) async fn cancel_known_task(
        self: &Arc<Self>,
        network: &str,
        scope: &str,
        task_ref: &str,
        status: &Value,
    ) -> Result<Value> {
        let target = field(&status["task"], "target_node")?;
        let source = field(status, "source_node")?;
        let local = self.identity.peer_id()?;
        if target == local {
            self.cancel_agent_task(network, scope, task_ref, source)
                .await
        } else {
            ensure!(
                source == local,
                "cancel this task through its source or target device"
            );
            self.peer_request(
                network,
                target,
                json!({"type":"cancel_agent_task","conversation_id":scope,"task_ref":task_ref}),
            )
            .await
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remote_native_snapshot_survives_reopen_and_unavailable_reads_without_controls() -> Result<()>
    {
        let dir = crate::runtime::tests::fixture()?;
        let identity = LocalIdentity::generate();
        let network = identity.fixture_network_id()?;
        let mut store = Store::open(dir.join("node.sqlite3"))?;
        store.install_trust_bundle(
            &network,
            &identity.sign_network_fixture(
                1,
                vec![identity.certificate(vec![NodeRole::Replica, NodeRole::Scheduler])?],
            )?,
        )?;
        let session = json!({"conversation_id":"chat","target_node":"remote","work_id":"work",
            "task_ref":"task","thread_id":"thread","turn_id":"turn","state":"waiting","detail":"original work",
            "freshness":"observed","can_control":true,
            "pending_requests":[{"request_id":7,"kind":"item/commandExecution/requestApproval","text":"confirm the original command",
                "choices":[{"label":"拒绝","response":{"decision":"decline"}}]}]});
        let live = remote_native_projection(
            &mut store,
            &network,
            "chat",
            "remote",
            Ok(json!({"native_sessions":[session]})),
        )?;
        assert_eq!(live[0]["freshness"], "observed");
        assert_eq!(live[0]["can_control"], true);
        assert_eq!(live[0]["pending_requests"][0]["request_id"], 7);
        drop(store);
        let mut store = Store::open(dir.join("node.sqlite3"))?;
        let cached = remote_native_projection(
            &mut store,
            &network,
            "chat",
            "remote",
            Err(anyhow::anyhow!("peer unavailable")),
        )?;
        assert_eq!(cached.len(), 1);
        assert_eq!(cached[0]["work_id"], "work");
        assert_eq!(cached[0]["thread_id"], "thread");
        assert_eq!(cached[0]["turn_id"], "turn");
        assert_eq!(cached[0]["freshness"], "cached");
        assert_eq!(cached[0]["can_control"], false);
        assert_eq!(cached[0]["state"], "unknown");
        assert_eq!(cached[0]["pending_requests"], session["pending_requests"]);
        let wrong_scope = json!({"native_sessions":[{"conversation_id":"other","target_node":"remote","work_id":"foreign","thread_id":"foreign"}]});
        assert_eq!(
            remote_native_projection(&mut store, &network, "chat", "remote", Ok(wrong_scope))?[0]["work_id"],
            "work"
        );
        assert!(
            remote_native_projection(
                &mut store,
                &network,
                "other",
                "remote",
                Err(anyhow::anyhow!("peer unavailable"))
            )?
            .is_empty()
        );
        let mut invalid_control = session.clone();
        invalid_control["can_control"] = json!("true");
        let invalid = remote_native_projection(
            &mut store,
            &network,
            "chat",
            "remote",
            Ok(json!({"native_sessions":[invalid_control]})),
        )?;
        assert_eq!(invalid[0]["freshness"], "cached");
        assert_eq!(invalid[0]["can_control"], false);
        let mut completed = session;
        completed["state"] = json!("completed");
        completed["can_control"] = json!(false);
        completed["pending_requests"] = json!([]);
        remote_native_projection(
            &mut store,
            &network,
            "chat",
            "remote",
            Ok(json!({"native_sessions":[completed]})),
        )?;
        let terminal = remote_native_projection(
            &mut store,
            &network,
            "chat",
            "remote",
            Err(anyhow::anyhow!("peer unavailable")),
        )?;
        assert_eq!(terminal[0]["state"], "completed");
        assert_eq!(terminal[0]["freshness"], "cached");
        assert_eq!(terminal[0]["can_control"], false);
        drop(store);
        std::fs::remove_dir_all(dir)?;
        Ok(())
    }
}
