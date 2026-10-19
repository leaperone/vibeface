use super::*;
use crate::scheduling::{JobOutcome, JobState, TriggerKind};
use serde::{Deserialize, Serialize};

const INDEX: &str = "native-observation:v1:index";
const MAX_NATIVE_WORK: usize = 256;

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum NativeState {
    Running,
    Waiting,
    Interrupting,
    Completed,
    Failed,
    Interrupted,
    Unknown,
}

impl NativeState {
    pub(super) fn terminal(self) -> bool {
        matches!(self, Self::Completed | Self::Failed | Self::Interrupted)
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct NativeWork {
    pub work_id: String,
    pub conversation_id: String,
    pub task_ref: Option<String>,
    pub origin_trigger: String,
    pub attempt_id: String,
    pub target_node: String,
    pub thread_id: String,
    pub turn_id: Option<String>,
    pub state: NativeState,
    pub detail: String,
    #[serde(default)]
    pub sequence: u64,
}

fn key(work_id: &str) -> String {
    format!("native-observation:v1:work:{work_id}")
}

pub(super) fn sessions_from(store: &Store, network: &str) -> Result<Vec<NativeWork>> {
    let ids: Vec<String> =
        serde_json::from_value(store.local_value(network, INDEX)?.unwrap_or(json!([])))?;
    ids.into_iter()
        .map(|id| {
            serde_json::from_value(
                store
                    .local_value(network, &key(&id))?
                    .context("native observation is missing")?,
            )
            .map_err(Into::into)
        })
        .collect()
}

fn save_work(store: &mut Store, network: &str, work: &NativeWork) -> Result<()> {
    store.local_set(network, &key(&work.work_id), &serde_json::to_value(work)?)
}

fn event_identity(event: &Value) -> (Option<&str>, Option<&str>) {
    let params = &event["params"];
    (
        params["threadId"]
            .as_str()
            .or_else(|| params["thread"]["id"].as_str()),
        params["turnId"]
            .as_str()
            .or_else(|| params["turn"]["id"].as_str()),
    )
}

fn terminal_state(status: &str) -> Option<NativeState> {
    match status {
        "completed" => Some(NativeState::Completed),
        "failed" => Some(NativeState::Failed),
        "interrupted" => Some(NativeState::Interrupted),
        _ => None,
    }
}

fn apply_event(work: &mut NativeWork, event: &Value) -> bool {
    let (thread, turn) = event_identity(event);
    if work.state.terminal()
        || thread != Some(work.thread_id.as_str())
        || turn.is_none()
        || turn != work.turn_id.as_deref()
    {
        return false;
    }
    let params = &event["params"];
    match event["method"].as_str() {
        Some("turn/completed") => {
            let Some(state) = params["turn"]["status"].as_str().and_then(terminal_state) else {
                return false;
            };
            work.state = state;
            if let Some(error) = params["turn"]["error"]["message"].as_str() {
                work.detail = error.chars().take(65536).collect();
            }
        }
        Some("item/agentMessage/delta") => {
            if work.state != NativeState::Interrupting {
                work.state = NativeState::Running;
            }
            if let Some(text) = params["delta"].as_str() {
                work.detail = format!("{}{text}", work.detail)
                    .chars()
                    .take(65536)
                    .collect();
            }
        }
        Some("item/completed") if params["item"]["type"] == "agentMessage" => {
            if work.state != NativeState::Interrupting {
                work.state = NativeState::Running;
            }
            if let Some(text) = params["item"]["text"].as_str() {
                work.detail = text.chars().take(65536).collect();
            }
        }
        Some(method)
            if method.ends_with("requestApproval") || method == "item/tool/requestUserInput" =>
        {
            work.state = NativeState::Waiting;
        }
        Some("turn/started") => work.state = NativeState::Running,
        _ => {}
    }
    true
}

fn loaded_thread(thread: &Value) -> bool {
    matches!(
        thread["status"]["type"].as_str(),
        Some("idle" | "active" | "systemError")
    )
}

pub(crate) fn outcome(works: &[NativeWork]) -> Option<JobOutcome> {
    if works.is_empty() {
        return Some(JobOutcome::Completed);
    }
    if works.iter().any(|work| !work.state.terminal()) {
        return None;
    }
    if let Some(work) = works.iter().find(|work| work.state == NativeState::Failed) {
        return Some(JobOutcome::Failed(work.detail.clone()));
    }
    if works
        .iter()
        .any(|work| work.state == NativeState::Interrupted)
    {
        return Some(JobOutcome::Cancelled(
            "the native Socket confirmed the original turn was interrupted".into(),
        ));
    }
    Some(JobOutcome::Completed)
}

impl Runtime {
    fn native_client(&self) -> Result<&vibeface_bridge::codex::CodexClient> {
        self.codex
            .as_ref()
            .context("this Node has no native Codex Socket configured")
    }

    pub(super) async fn observe_session(&self, access: &Access, args: &Value) -> Result<Value> {
        let manager = access
            .manager
            .as_ref()
            .context("native observation requires a live Agent invocation")?;
        let scope = access
            .conversation_id
            .as_deref()
            .context("missing conversation")?;
        let thread_id = id(args, "thread_id")?;
        let turn_id = id(args, "turn_id")?;
        let client = self.native_client()?;
        let read = client
            .request(
                "thread/read",
                json!({"threadId":thread_id,"includeTurns":false}),
            )
            .await?;
        ensure!(
            read["thread"]["id"] == thread_id,
            "native Socket returned a different thread"
        );
        ensure!(
            loaded_thread(&read["thread"]),
            "the native thread is not loaded on this Socket; the owning process must expose its control endpoint"
        );
        let snapshot = client.snapshot().await;
        let cached = &snapshot["threads"][&thread_id]["last_turn"];
        let turn = if cached["id"] == turn_id {
            Some(cached.clone())
        } else {
            client.request("thread/turns/list",json!({"threadId":thread_id,"limit":100,"itemsView":"full","sortDirection":"desc"}))
                .await.ok().and_then(|reply| reply["data"].as_array().and_then(|turns|
                    turns.iter().find(|turn| turn["id"] == turn_id).cloned()))
        }.context("the original turn is not observed on this native Socket; expose its owning process before registering work")?;
        let observed_text = snapshot["threads"][&thread_id]["last_assistant"]
            .as_str()
            .unwrap_or("")
            .to_owned();
        let work = {
            let mut store = self.store.lock().await;
            Self::check_manager_permission(&store, access, true, None)?;
            let job = store
                .scheduler_job(
                    &access.network_id,
                    scope,
                    &manager.trigger,
                    &manager.target_node,
                )?
                .context("Agent invocation is missing")?;
            let task_ref = (job.kind == TriggerKind::AgentTask).then(|| job.trigger.clone());
            let works = sessions_from(&store, &access.network_id)?;
            if let Some(previous) = works.iter().find(|work| {
                work.thread_id == thread_id && work.turn_id.as_deref() == Some(&turn_id)
            }) {
                ensure!(
                    previous.conversation_id == scope
                        && previous.origin_trigger == manager.trigger
                        && previous.attempt_id == manager.attempt_id,
                    "this native turn already belongs to another task or invocation"
                );
                return Ok(json!(previous));
            }
            ensure!(
                works.iter().filter(|work| !work.state.terminal()).count() < MAX_NATIVE_WORK,
                "native observation capacity is full"
            );
            let work = NativeWork {
                work_id: crypto::digest(&serde_json::to_vec(&(
                    scope,
                    &manager.attempt_id,
                    &thread_id,
                    &turn_id,
                ))?),
                conversation_id: scope.into(),
                task_ref,
                origin_trigger: manager.trigger.clone(),
                attempt_id: manager.attempt_id.clone(),
                target_node: self.identity.peer_id()?,
                thread_id,
                turn_id: Some(turn_id),
                state: NativeState::Running,
                detail: observed_text,
                sequence: 0,
            };
            save_work(&mut store, &access.network_id, &work)?;
            let mut ids = works
                .into_iter()
                .map(|work| work.work_id)
                .collect::<Vec<_>>();
            ids.push(work.work_id.clone());
            store.local_set(&access.network_id, INDEX, &json!(ids))?;
            work
        };
        self.refresh_native_subscriptions().await?;
        self.observe_native(
            json!({"method":"turn/completed","params":{"threadId":work.thread_id,"turn":turn}}),
        )
        .await?;
        for request in snapshot["pending_requests"]
            .as_array()
            .into_iter()
            .flatten()
        {
            if event_identity(request) == (Some(work.thread_id.as_str()), work.turn_id.as_deref()) {
                self.observe_native(request.clone()).await?;
            }
        }
        self.publish_changed(&access.network_id, scope, &work.origin_trigger);
        self.store
            .lock()
            .await
            .local_value(&access.network_id, &key(&work.work_id))?
            .context("native observation was not saved")
    }

    pub(super) fn start_native_observation(self: &Arc<Self>) {
        let node = self.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(2));
            loop {
                tick.tick().await;
                let _work = node.maintenance.enter_when_open().await;
                if node.codex.is_some()
                    && let Err(error) = node.refresh_native_subscriptions().await
                {
                    tracing::debug!(%error, "native observation recovery is pending");
                }
                if let Err(error) = crate::scheduler::recover_native_results(node.clone()).await {
                    tracing::debug!(%error, "native result finalization is pending");
                }
            }
        });
    }

    async fn refresh_native_subscriptions(&self) -> Result<()> {
        let client = self.native_client()?;
        let networks = self.store.lock().await.networks()?;
        let mut all = Vec::new();
        for network in networks {
            let store = self.store.lock().await;
            if store
                .authorize_node(
                    &network,
                    &self.identity.peer_id()?,
                    Some(NodeRole::Scheduler),
                )
                .is_ok()
            {
                all.extend(
                    sessions_from(&store, &network)?
                        .into_iter()
                        .filter(|work| !work.state.terminal())
                        .map(|work| (network.clone(), work)),
                );
            }
        }
        client
            .set_observed_threads(all.iter().map(|(_, work)| work.thread_id.clone()).collect())?;
        let snapshot = client.snapshot().await;
        if snapshot["connection"]["state"] != "connected" {
            return Ok(());
        }
        for (network, mut work) in all {
            let thread = &snapshot["threads"][&work.thread_id];
            if !loaded_thread(thread) {
                if work.state != NativeState::Unknown {
                    work.state = NativeState::Unknown;
                    save_work(&mut *self.store.lock().await, &network, &work)?;
                    self.publish_changed(&network, &work.conversation_id, &work.origin_trigger);
                }
                continue;
            }
            let turn = &thread["last_turn"];
            if turn["id"].as_str() != work.turn_id.as_deref() {
                continue;
            }
            if let Some(state) = turn["status"].as_str().and_then(terminal_state) {
                if let Some(text) = thread["last_assistant"].as_str() {
                    work.detail = text.chars().take(65536).collect();
                }
                work.state = state;
                let mut store = self.store.lock().await;
                let current: NativeWork = serde_json::from_value(
                    store
                        .local_value(&network, &key(&work.work_id))?
                        .context("native work is missing")?,
                )?;
                if !current.state.terminal() {
                    save_work(&mut store, &network, &work)?;
                }
                drop(store);
                self.publish_changed(&network, &work.conversation_id, &work.origin_trigger);
            }
        }
        Ok(())
    }

    pub(super) async fn observe_native(&self, event: Value) -> Result<()> {
        let _work = self.maintenance.enter_when_open().await;
        let networks = self.store.lock().await.networks()?;
        for network in networks {
            let mut store = self.store.lock().await;
            if store
                .authorize_node(
                    &network,
                    &self.identity.peer_id()?,
                    Some(NodeRole::Scheduler),
                )
                .is_err()
            {
                continue;
            }
            let works = sessions_from(&store, &network)?;
            for mut work in works {
                if event["method"] == "bridge.codex_disconnected" && !work.state.terminal() {
                    work.state = NativeState::Unknown;
                    save_work(&mut store, &network, &work)?;
                } else if apply_event(&mut work, &event) {
                    work.sequence = work
                        .sequence
                        .checked_add(1)
                        .context("native progress sequence overflow")?;
                    save_work(&mut store, &network, &work)?;
                    let sequence = work.sequence;
                    if event.get("id").is_some()
                        && serde_json::from_value::<vibeface_protocol::RpcId>(event["id"].clone())
                            .is_ok()
                        && event["method"].as_str().is_some_and(|method| {
                            method.ends_with("requestApproval")
                                || method == "item/tool/requestUserInput"
                        })
                    {
                        store.queue_notification(
                            &network,
                            &Notification {
                                notification_id: format!(
                                    "native-input-{}",
                                    crypto::digest(&serde_json::to_vec(&(
                                        &work.work_id,
                                        &event["id"]
                                    ))?)
                                ),
                                conversation_id: work.conversation_id.clone(),
                                kind: "needs_input".into(),
                                text: "后台工作需要你的确认。".into(),
                                event_hash: work.origin_trigger.clone(),
                            },
                        )?;
                    }
                    self.publish_progress(&network, json!({"type":"native_progress","conversation_id":work.conversation_id,
                        "work_id":work.work_id,"target_node":work.target_node,"source_node":work.target_node,
                        "thread_id":work.thread_id,"turn_id":work.turn_id,"task_ref":work.task_ref,
                        "sequence":sequence,"progress":observation::native_progress(&event)}));
                } else {
                    continue;
                }
                self.publish_changed(&network, &work.conversation_id, &work.origin_trigger);
            }
        }
        Ok(())
    }

    pub(crate) async fn wait_native_work(
        &self,
        network: &str,
        scope: &str,
        trigger: &str,
        attempt: &str,
    ) -> Result<Vec<NativeWork>> {
        loop {
            let works = self
                .native_work_for_attempt(network, scope, trigger, attempt)
                .await?;
            let target = self.identity.peer_id()?;
            let job = self
                .store
                .lock()
                .await
                .scheduler_job(network, scope, trigger, &target)?
                .context("task invocation is missing")?;
            let children = if job.kind == TriggerKind::AgentTask {
                self.child_agent_tasks(network, scope, trigger).await?
            } else {
                Vec::new()
            };
            if outcome(&works).is_some()
                && children.iter().all(|task| {
                    matches!(
                        task["state"].as_str(),
                        Some("completed" | "failed" | "interrupted" | "cancelled")
                    )
                })
            {
                return Ok(works);
            }
            if matches!(
                job.state,
                JobState::Cancelling { .. } | JobState::Unknown { .. }
            ) {
                return Err(crate::manager::ManagerError {
                    kind: crate::manager::ManagerFailure::Unknown, session_id: None,
                    message: "native work has no confirmed terminal outcome; the original turn was retained for reconciliation".into(),
                }.into());
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    }

    pub(crate) async fn child_agent_tasks(
        &self,
        network: &str,
        scope: &str,
        trigger: &str,
    ) -> Result<Vec<Value>> {
        let local = self.identity.peer_id()?;
        let store = self.store.lock().await;
        let context = store.context(network, scope, &[])?;
        context
            .events
            .iter()
            .filter_map(|event| {
                if event.scope_id != scope || event.author != local {
                    return None;
                }
                let EventBody::AgentMessage { message } = &event.body else {
                    return None;
                };
                (message.kind == AgentMessageKind::Task && message.origin_trigger == trigger).then(
                    || {
                        store.agent_task_status(
                            network,
                            scope,
                            &crypto::digest(&serde_json::to_vec(event)?),
                        )
                    },
                )
            })
            .collect()
    }

    pub(crate) async fn native_work_for_attempt(
        &self,
        network: &str,
        scope: &str,
        trigger: &str,
        attempt: &str,
    ) -> Result<Vec<NativeWork>> {
        Ok(sessions_from(&*self.store.lock().await, network)?
            .into_iter()
            .filter(|work| {
                work.conversation_id == scope
                    && work.origin_trigger == trigger
                    && work.attempt_id == attempt
            })
            .collect())
    }

    pub(super) async fn native_status(&self, network: &str, scope: &str) -> Result<Value> {
        let snapshot = match &self.codex {
            Some(client) => client.snapshot().await,
            None => Value::Null,
        };
        let store = self.store.lock().await;
        store.authorize_node(network, &self.identity.peer_id()?, None)?;
        let sessions = sessions_from(&store, network)?
            .into_iter()
            .filter(|work| work.conversation_id == scope)
            .map(|work| {
                let mut value = json!(work);
                value["pending_requests"] = json!(super::chat_controls::pending_requests(
                    &snapshot,
                    Some(&work.thread_id),
                    work.turn_id.as_deref()
                ));
                value
            })
            .collect::<Vec<_>>();
        Ok(json!({"native_sessions":sessions}))
    }

    pub(super) async fn native_control(
        self: &Arc<Self>,
        access: &Access,
        input: &Value,
    ) -> Result<Value> {
        ensure!(access.human, "native controls require the user");
        let scope = field(input, "conversation_id")?;
        let target = field(input, "target_node")?;
        self.require_scope(access, scope).await?;
        let method = field(input, "method")?;
        if target != self.identity.peer_id()? {
            self.require_role(&access.network_id, NodeRole::Gateway)
                .await?;
            let mut request = input.clone();
            request["type"] = json!(method);
            request["human"] = json!(true);
            return self.peer_request(&access.network_id, target, request).await;
        }
        self.native_control_local(&access.network_id, input, method)
            .await
    }

    pub(super) async fn native_control_local(
        &self,
        network: &str,
        input: &Value,
        method: &str,
    ) -> Result<Value> {
        let scope = field(input, "conversation_id")?;
        let work_id = field(input, "work_id")?;
        ensure!(
            field(input, "target_node")? == self.identity.peer_id()?,
            "native control target mismatch"
        );
        let work: NativeWork = {
            let store = self.store.lock().await;
            store.authorize_node(
                network,
                &self.identity.peer_id()?,
                Some(NodeRole::Scheduler),
            )?;
            serde_json::from_value(
                store
                    .local_value(network, &key(work_id))?
                    .context("native work not found")?,
            )?
        };
        ensure!(
            work.conversation_id == scope && work.target_node == self.identity.peer_id()?,
            "native control scope mismatch"
        );
        ensure!(!work.state.terminal(), "native turn is already terminal");
        if method == "native_interrupt" {
            return self.interrupt_native_work(network, work).await;
        }
        ensure!(method == "native_respond", "unknown native control");
        let request_id = input
            .get("request_id")
            .context("native request ID is required")?;
        serde_json::from_value::<vibeface_protocol::RpcId>(request_id.clone())?;
        let snapshot = self.native_client()?.snapshot().await;
        let request = snapshot["pending_requests"]
            .as_array()
            .into_iter()
            .flatten()
            .find(|request| {
                &request["id"] == request_id
                    && event_identity(request)
                        == (Some(work.thread_id.as_str()), work.turn_id.as_deref())
            })
            .context("the native request no longer belongs to the original turn")?;
        validate_response(request, &input["response"])?;
        self.native_client()?
            .respond(request_id.clone(), input["response"].clone())
            .await?;
        Ok(json!({"work_id":work_id,"responded":true}))
    }

    async fn interrupt_native_work(&self, network: &str, mut work: NativeWork) -> Result<Value> {
        let turn = work
            .turn_id
            .as_deref()
            .context("native turn is unknown; verify it on the owning device")?;
        let client = self.native_client()?;
        let snapshot = client.snapshot().await;
        ensure!(
            snapshot["connection"]["state"] == "connected"
                && loaded_thread(&snapshot["threads"][&work.thread_id]),
            "native turn cannot be controlled on this Socket; its outcome is unknown"
        );
        ensure!(
            snapshot["threads"][&work.thread_id]["last_turn"]["id"].as_str() == Some(turn),
            "native thread now points at a different turn; refresh its original work before interrupting"
        );
        client
            .request(
                "turn/interrupt",
                json!({"threadId":work.thread_id,"turnId":turn}),
            )
            .await?;
        work.state = NativeState::Interrupting;
        let mut store = self.store.lock().await;
        let current: NativeWork = serde_json::from_value(
            store
                .local_value(network, &key(&work.work_id))?
                .context("native work missing")?,
        )?;
        if !current.state.terminal() {
            save_work(&mut store, network, &work)?;
        }
        drop(store);
        self.publish_changed(network, &work.conversation_id, &work.origin_trigger);
        Ok(json!({"work_id":work.work_id,"interrupt_requested":true,"confirmed":false}))
    }

    pub(super) async fn interrupt_native_task(
        &self,
        network: &str,
        scope: &str,
        trigger: &str,
    ) -> Result<()> {
        let works = sessions_from(&*self.store.lock().await, network)?
            .into_iter()
            .filter(|work| {
                work.conversation_id == scope
                    && work.origin_trigger == trigger
                    && !work.state.terminal()
            })
            .collect::<Vec<_>>();
        for work in works {
            if let Err(error) = self.interrupt_native_work(network, work).await {
                tracing::debug!(%error, "native task interruption is unconfirmed");
            }
        }
        Ok(())
    }
}

fn validate_response(request: &Value, response: &Value) -> Result<()> {
    match request["method"].as_str() {
        Some("item/commandExecution/requestApproval" | "item/fileChange/requestApproval") => {
            let decision = response["decision"]
                .as_str()
                .context("approval decision is required")?;
            ensure!(
                matches!(decision, "accept" | "decline" | "cancel"),
                "unsupported approval decision"
            );
            ensure!(
                request["params"]["availableDecisions"]
                    .as_array()
                    .is_none_or(|allowed| allowed
                        .iter()
                        .any(|allowed| allowed.as_str() == Some(decision))),
                "the native request does not offer that decision"
            );
        }
        Some("item/tool/requestUserInput") => {
            let answers = response["answers"]
                .as_object()
                .context("answers are required")?;
            let questions = request["params"]["questions"]
                .as_array()
                .context("native input questions are missing")?;
            ensure!(
                answers.len() == questions.len(),
                "answer every native question exactly once"
            );
            for question in questions {
                let id = question["id"]
                    .as_str()
                    .context("native question ID is missing")?;
                let values = answers
                    .get(id)
                    .and_then(|answer| answer["answers"].as_array())
                    .context("native answer is missing")?;
                ensure!(
                    !values.is_empty() && values.iter().all(Value::is_string),
                    "native answers must be text"
                );
            }
        }
        _ => bail!("this native request must be answered on the owning device"),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn work() -> NativeWork {
        NativeWork {
            work_id: "work".into(),
            conversation_id: "chat".into(),
            task_ref: Some("task".into()),
            origin_trigger: "task".into(),
            attempt_id: "attempt".into(),
            target_node: "node".into(),
            thread_id: "thread".into(),
            turn_id: Some("turn".into()),
            state: NativeState::Running,
            detail: String::new(),
            sequence: 0,
        }
    }

    #[test]
    fn terminal_events_require_the_original_thread_and_turn_and_are_idempotent() {
        let mut work = work();
        for (thread, turn) in [("other", "turn"), ("thread", "other")] {
            assert!(!apply_event(
                &mut work,
                &json!({"method":"turn/completed","params":{"threadId":thread,"turn":{"id":turn,"status":"completed"}}})
            ));
            assert_eq!(work.state, NativeState::Running);
        }
        assert!(!apply_event(
            &mut work,
            &json!({"method":"thread/status/changed","params":{"threadId":"thread","status":{"type":"idle"}}})
        ));
        let completed = json!({"method":"turn/completed","params":{"threadId":"thread","turn":{"id":"turn","status":"completed"}}});
        assert!(apply_event(&mut work, &completed));
        assert!(!apply_event(&mut work, &completed));
        assert!(matches!(outcome(&[work]), Some(JobOutcome::Completed)));
    }

    #[test]
    fn native_progress_sequence_survives_store_reopen() -> Result<()> {
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
        let mut observed = work();
        observed.sequence = 41;
        save_work(&mut store, &network, &observed)?;
        store.local_set(&network, INDEX, &json!([observed.work_id]))?;
        drop(store);
        let store = Store::open(dir.join("node.sqlite3"))?;
        let restored = sessions_from(&store, &network)?;
        assert_eq!(restored.len(), 1);
        assert_eq!(restored[0].sequence, 41);
        assert_eq!(restored[0].thread_id, "thread");
        assert_eq!(restored[0].turn_id.as_deref(), Some("turn"));
        drop(store);
        std::fs::remove_dir_all(dir)?;
        Ok(())
    }

    #[test]
    fn cancellation_and_approval_require_real_native_evidence() -> Result<()> {
        let mut interrupted = work();
        interrupted.state = NativeState::Interrupting;
        assert!(outcome(&[interrupted.clone()]).is_none());
        interrupted.state = NativeState::Interrupted;
        assert!(matches!(
            outcome(&[interrupted]),
            Some(JobOutcome::Cancelled(_))
        ));
        let approval = json!({"method":"item/commandExecution/requestApproval","params":{"availableDecisions":["decline"]}});
        assert!(validate_response(&approval, &json!({"decision":"accept"})).is_err());
        validate_response(&approval, &json!({"decision":"decline"}))?;
        assert!(!loaded_thread(
            &json!({"status":{"type":"notLoaded"},"turns":[{"id":"turn","status":"completed"}]})
        ));
        Ok(())
    }
}
