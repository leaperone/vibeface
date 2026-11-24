//! Caller-keyed local execution. Only the sidecar owns gateway work and its native bindings.
use super::{
    Runtime,
    execution_directory::CanonicalExecutionPath,
    executor::{SteerAck, SteerInput, SteerTrace},
};
use crate::{
    clock::now_ms,
    model::{ResolvedModel, WorkKind},
    providers::Provider,
    scheduling::SteerTarget,
    work::{Probe, Signal, WorkRecord, WorkSpec, WorkState},
};
use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{fmt, path::Path, sync::Arc, time::Duration};
use tokio::time::Instant;
use vibeface_bridge::failure::{ExecFailure, FailureKind};

pub(crate) mod store;
#[cfg(test)]
mod tests;
pub(crate) use store::Caller;
use store::Entry;

pub(super) const GATEWAY_NETWORK: &str = "executor-gateway";
const MAX_WAIT_MS: u64 = 120_000;

#[derive(Debug)]
pub(crate) struct GatewayError {
    pub code: &'static str,
    pub message: String,
}

impl fmt::Display for GatewayError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for GatewayError {}

pub(super) fn err(code: &'static str, msg: &str) -> anyhow::Error {
    GatewayError {
        code,
        message: msg.into(),
    }
    .into()
}

fn parse<T: serde::de::DeserializeOwned>(input: Value) -> Result<T> {
    serde_json::from_value(input).map_err(|e| err("invalid_request", &e.to_string()))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Start {
    call_ref: String,
    provider: String,
    cwd: String,
    prompt: String,
    title: Option<String>,
    model: Option<String>,
    effort: Option<String>,
    #[serde(default)]
    write: bool,
    thread: Option<String>,
    continues: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Work {
    work_id: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Wait {
    work_id: String,
    timeout_ms: u64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Queue {
    work_id: String,
    call_ref: String,
    prompt: String,
    timeout_ms: u64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Steer {
    work_id: String,
    call_ref: String,
    prompt: String,
}

fn validate_input(reference: &str, prompt: &str) -> Result<()> {
    if !vibeface_protocol::valid_business_id(reference) {
        return Err(err(
            "invalid_request",
            "call_ref must be a valid identifier (1–128 ASCII characters)",
        ));
    }
    if prompt.trim().is_empty() || prompt.len() > 65536 {
        return Err(err(
            "invalid_request",
            "prompt must contain text and fit in 65536 bytes",
        ));
    }
    Ok(())
}

fn deadline(ms: u64) -> Result<Instant> {
    if !(1..=MAX_WAIT_MS).contains(&ms) {
        return Err(err(
            "invalid_request",
            "timeout_ms must be between 1 and 120000",
        ));
    }
    Ok(Instant::now() + Duration::from_millis(ms))
}

#[derive(Clone, Debug, Default, PartialEq, Deserialize, Serialize)]
struct TurnDetails {
    changed_files: Vec<String>,
    failed_commands: Vec<Value>,
}

impl TurnDetails {
    fn codex(turn: &Value) -> Self {
        let mut details = Self::default();
        let Some(items) = turn["items"].as_array() else {
            return details;
        };
        for item in items {
            if item["type"] == "fileChange" && item["status"] == "completed" {
                for path in item["changes"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|change| change["path"].as_str())
                {
                    if details.changed_files.len() >= 128 {
                        break;
                    }
                    let path = path.chars().take(1024).collect::<String>();
                    if !details.changed_files.contains(&path) {
                        details.changed_files.push(path);
                    }
                }
            }
        }
        for item in items
            .iter()
            .rev()
            .filter(|item| {
                item["type"] == "commandExecution"
                    && (item["status"] == "failed"
                        || item["exitCode"].as_i64().is_some_and(|code| code != 0))
            })
            .take(5)
        {
            let clip = |field: &str, limit| {
                item[field]
                    .as_str()
                    .unwrap_or_default()
                    .chars()
                    .take(limit)
                    .collect::<String>()
            };
            details.failed_commands.push(json!({"command":clip("command",512),"exit_code":item["exitCode"].as_i64(),"output":clip("aggregatedOutput",1024)}));
        }
        details.failed_commands.reverse();
        details
    }
}

fn waiting_for_input(record: &WorkRecord) -> bool {
    record.progress == "needs_input" && !matches!(record.state, WorkState::Unverified { .. })
}

pub(crate) fn sandbox_label(write: bool) -> &'static str {
    if write {
        "danger-full-access"
    } else {
        "read-only"
    }
}

impl Runtime {
    fn enter_gateway(&self) -> Result<crate::maintenance::WorkGuard> {
        self.maintenance.enter().map_err(|error| {
            if error.downcast_ref::<crate::maintenance::Busy>().is_some() {
                err(
                    "unavailable",
                    "node is preparing an update; try again after it restarts",
                )
            } else {
                error
            }
        })
    }

    pub(crate) async fn gateway_call(
        self: &Arc<Self>,
        caller: &Caller,
        op: &str,
        mut args: Value,
    ) -> Result<Value> {
        let object = args
            .as_object_mut()
            .ok_or_else(|| err("invalid_request", "expected a JSON object"))?;
        let local = self.identity.device_id()?;
        for key in ["node", "target", "target_node", "node_id"] {
            if let Some(target) = object.remove(key)
                && !matches!(target.as_str(), Some(id) if id == local || id == "local")
            {
                return Err(err(
                    "cross_node_deferred",
                    "the gateway executes only on this Node",
                ));
            }
        }
        // The HTTP boundary already admits through maintenance::Gate, then drops
        // the lease for wait/queue/MCP. Re-enter here so direct gateway_call
        // (tests, future maintenance-socket methods) uses the same Gate.
        // Drop the lease before a possibly long wait; queue re-enters to start.
        let _work = match op {
            "start" | "steer" | "stop" => Some(self.enter_gateway()?),
            _ => {
                drop(self.enter_gateway()?);
                None
            }
        };
        match op {
            "capabilities" => Ok(self.gateway_capabilities(caller)),
            "start" => self.gateway_start(caller, parse(args)?),
            "status" | "result" => {
                let request: Work = parse(args)?;
                Ok(self.gateway.get(caller, &request.work_id)?.compact())
            }
            "wait" => {
                let request: Wait = parse(args)?;
                let entry = self
                    .gateway_wait(caller, &request.work_id, deadline(request.timeout_ms)?)
                    .await?;
                Ok(entry.compact())
            }
            "queue" => self.gateway_queue(caller, parse(args)?).await,
            "steer" => self.gateway_steer(caller, parse(args)?).await,
            "stop" => self.gateway_stop(caller, parse(args)?).await,
            _ => Err(err("invalid_request", "unknown gateway operation")),
        }
    }

    fn gateway_capabilities(&self, caller: &Caller) -> Value {
        let local = self.providers();
        let mut providers = serde_json::Map::new();
        for provider in [Provider::Codex, Provider::Claude] {
            providers.insert(provider.name().into(), json!({
                "enabled":local.switches.execution.contains(&provider),
                "operations":{"start":true,"status":true,"wait":true,"steer":true,"queue":true,"stop":true,"result":true},
                "sandbox_modes":["read-only","danger-full-access"],"default_sandbox":"read-only",
                "sandbox_description":match provider {
                    Provider::Claude => "Claude CLI; read-only: --restricted --safe-mode --strict-mcp-config --permission-mode dontAsk --tools Read,Glob,Grep; danger-full-access: --permission-mode bypassPermissions",
                    _ => "Codex native read-only or danger-full-access, approvalPolicy=never; read-only disables network"
                },
                "images":false,"effort":true,"model":true,"local_only":true,"file_and_command_results":provider == Provider::Codex
            }));
        }
        json!({"local_only":true,"cross_node":false,"default_sandbox":"read-only","providers":providers,
            "can_write":caller.can_write,"limits":{"max_wait_ms":MAX_WAIT_MS,"max_prompt_bytes":65536,
            "attachments":false,"max_changed_files":128,"max_failed_commands":5,"max_command_output_chars":1024,"interactive_answers":false}})
    }

    fn gateway_start(self: &Arc<Self>, caller: &Caller, request: Start) -> Result<Value> {
        validate_input(&request.call_ref, &request.prompt)?;
        let provider = match request.provider.as_str() {
            "codex" => Provider::Codex,
            "claude" => Provider::Claude,
            _ => {
                return Err(err(
                    "unsupported_provider",
                    "gateway supports codex and claude",
                ));
            }
        };
        if let Some(model) = &request.model {
            crate::model::validate_model(model)
                .map_err(|e| err("invalid_request", &e.to_string()))?;
            if model.starts_with('-') {
                return Err(err("invalid_request", "invalid model name"));
            }
        }
        provider
            .validate_reasoning_effort(request.effort.as_deref())
            .map_err(|e| err("invalid_request", &e.to_string()))?;
        if request
            .title
            .as_ref()
            .is_some_and(|title| title.len() > 512)
        {
            return Err(err("invalid_request", "title exceeds 512 bytes"));
        }
        if request.write && !caller.can_write {
            return Err(err("write_forbidden", "caller lacks write permission"));
        }
        let path = CanonicalExecutionPath::resolve(Path::new(&request.cwd))
            .map_err(|_| err("cwd_not_allowed", "cwd must be an existing directory"))?
            .authorize(&self.settings_snapshot().roots)
            .map_err(|_| {
                err(
                    "cwd_not_allowed",
                    "cwd must be a directory inside a configured execution root",
                )
            })?;
        let cwd = path
            .to_str()
            .ok_or_else(|| err("cwd_not_allowed", "cwd must be UTF-8"))?
            .to_owned();
        if request.thread.is_some() && request.continues.is_some() {
            return Err(err("invalid_request", "specify either thread or continues"));
        }
        let previous = match (&request.continues, &request.thread) {
            (Some(id), _) => Some(self.gateway.get(caller, id)?),
            (_, Some(thread)) => Some(self.gateway.by_thread(caller, thread)?),
            _ => None,
        };
        let thread = if let Some(previous) = previous {
            if previous.spec.kind != WorkKind::Agent(provider)
                || previous.spec.cwd != cwd
                || previous.write != request.write
            {
                return Err(err(
                    "invalid_request",
                    "a continuation must keep its provider, cwd and sandbox",
                ));
            }
            Some(
                previous
                    .thread
                    .ok_or_else(|| err("thread_busy", "work has no journaled thread"))?,
            )
        } else {
            None
        };
        let local = self.providers();
        let model = local
            .switches
            .resolve_execution_model(provider, request.model.as_deref());
        let conversation = format!("gateway:{}", caller.id);
        let target = self.identity.device_id()?;
        let spec = WorkSpec {
            work_id: WorkSpec::id(&conversation, "gateway", &request.call_ref),
            conversation,
            trigger: "gateway".into(),
            dispatcher: target.clone(),
            target,
            call_ref: request.call_ref,
            kind: WorkKind::Agent(provider),
            model: request.model,
            resolved: ResolvedModel {
                provider,
                model: (!model.is_empty()).then_some(model),
            },
            cwd,
            title: request.title.unwrap_or_default(),
            prompt: request.prompt,
            continues: request.continues,
            parent: None,
            thread,
        };
        if !local.switches.execution.contains(&provider)
            && self.gateway.find(caller, &spec.work_id)?.is_none()
        {
            return Err(err(
                "provider_disabled",
                "executor is disabled on this Node",
            ));
        }
        let (entry, created) =
            self.gateway
                .accept(caller, &spec, request.write, request.effort.as_deref())?;
        if created {
            let node = self.clone();
            let task = entry.clone();
            self.spawn_background(async move {
                if let Err(error) = node.gateway_launch(&task).await {
                    tracing::warn!(work_id = task.spec.work_id, %error, "gateway could not journal executor start");
                }
            })?;
        }
        Ok(entry.compact())
    }

    async fn gateway_launch(self: &Arc<Self>, entry: &Entry) -> Result<()> {
        let work = &entry.spec;
        let provider = work.resolved.provider;
        let args = json!({"write":entry.write,"effort":entry.effort});
        let executor = self.executor(provider)?;
        let signal = match executor
            .start_or_resume(self, GATEWAY_NETWORK, work, work.thread.as_deref(), &args)
            .await
        {
            Ok(false) => return Ok(()),
            Ok(true) => self.gateway_codex_start(entry).await,
            Err(e)
                if e.downcast_ref::<GatewayError>()
                    .is_some_and(|e| e.code == "submission_unverified") =>
            {
                Signal::SubmissionUnverified(e.to_string())
            }
            Err(e) => Signal::StartRejected(
                e.downcast::<ExecFailure>()
                    .unwrap_or_else(|e| ExecFailure::new(FailureKind::Rejected, e.to_string())),
            ),
        };
        let record = self.gateway.signal(&work.work_id, signal)?;
        if provider == Provider::Codex && record.state.finished() {
            self.codex.release_work(&work.work_id);
        }
        Ok(())
    }

    pub(super) fn gateway_bind_thread(&self, work: &WorkSpec, thread: &str) -> Result<()> {
        anyhow::ensure!(
            self.providers()
                .switches
                .execution
                .contains(&work.resolved.provider),
            "executor was disabled before submission"
        );
        anyhow::ensure!(
            self.allowed_path(&work.cwd)? == Path::new(&work.cwd),
            "execution directory changed before submission"
        );
        let record = self
            .gateway
            .signal(&work.work_id, Signal::ThreadCreated(thread.into()))?;
        anyhow::ensure!(
            matches!(record.state, WorkState::Starting {thread: Some(ref bound)} if bound == thread)
                && now_ms() < record.send_by_ms,
            "native submission is no longer authorized"
        );
        Ok(())
    }

    async fn gateway_codex_start(&self, entry: &Entry) -> Signal {
        let work = &entry.spec;
        let client = self.codex.client();
        let mut params = json!({"cwd":work.cwd,"approvalPolicy":"never",
            "sandbox":sandbox_label(entry.write),
            "config":{"web_search":"disabled"}});
        if let Some(model) = work.execution_model() {
            params["model"] = json!(model);
        }
        let method = if let Some(thread) = &work.thread {
            params["threadId"] = json!(thread);
            "thread/resume"
        } else {
            params["threadSource"] = json!("vibeface-gateway");
            "thread/start"
        };
        let reply = match client.request(method, params).await {
            Ok(reply) => reply,
            Err(e) => return Signal::StartRejected(e.failure()),
        };
        let Some(thread) = reply["thread"]["id"].as_str().filter(|id| {
            vibeface_protocol::valid_business_id(id)
                && work.thread.as_deref().is_none_or(|old| old == *id)
        }) else {
            return Signal::StartRejected(ExecFailure::new(
                FailureKind::Rejected,
                "invalid thread id from Codex",
            ));
        };
        if let Err(error) = self.gateway_bind_thread(work, thread) {
            return Signal::StartRejected(ExecFailure::new(
                FailureKind::Rejected,
                error.to_string(),
            ));
        }
        let sandbox = if entry.write {
            json!({"type":"dangerFullAccess"})
        } else {
            json!({"type":"readOnly","networkAccess":false})
        };
        let mut turn = json!({"threadId":thread,"clientUserMessageId":work.work_id,"cwd":work.cwd,
            "approvalPolicy":"never","sandboxPolicy":sandbox,
            "input":[{"type":"text","text":work.prompt,"text_elements":[]}]});
        if let Some(model) = work.execution_model() {
            turn["model"] = json!(model);
        }
        if let Some(effort) = &entry.effort {
            turn["effort"] = json!(effort);
        }
        match client.request("turn/start", turn).await {
            Ok(reply) => match reply["turn"]["id"].as_str().filter(|id| !id.is_empty()) {
                Some(id) => Signal::TurnStarted {
                    turn_id: id.into(),
                    epoch: self.codex.epoch(),
                },
                None => Signal::SubmissionUnverified("Codex did not return a turn id".into()),
            },
            Err(e) if e.uncertain => {
                Signal::SubmissionUnverified("Codex turn submission was not confirmed".into())
            }
            Err(e) => Signal::StartRejected(e.failure()),
        }
    }

    pub(super) async fn gateway_codex_request(
        &self,
        request: &vibeface_bridge::codex::ServerRequest,
    ) -> bool {
        let Some(thread) = request.thread_id.as_deref() else {
            return false;
        };
        match self.gateway.native_thread(thread) {
            Ok(Some(entry)) if entry.spec.resolved.provider == Provider::Codex => {
                let active = entry.record.state.handle().is_none_or(|h| {
                    request.params["turnId"]
                        .as_str()
                        .is_none_or(|id| id == h.turn_id)
                });
                if active && !entry.record.state.finished() {
                    let _ = self.gateway.signal(
                        &entry.spec.work_id,
                        Signal::Progress {
                            summary: "needs_input".into(),
                            at_ms: now_ms(),
                        },
                    );
                }
            }
            Ok(_) => return false,
            Err(error) => tracing::warn!(%error,"cannot identify owner of Codex request"),
        }
        let (answer, decision) = vibeface_bridge::methods::fallback(&request.method);
        let _ = self
            .codex
            .client()
            .respond(&json!(request.id), answer, decision)
            .await;
        true
    }

    async fn gateway_wait(&self, caller: &Caller, id: &str, until: Instant) -> Result<Entry> {
        let mut changed = self.gateway.subscribe();
        loop {
            let entry = self.gateway.get(caller, id)?;
            if entry.record.state.finished()
                || waiting_for_input(&entry.record)
                || Instant::now() >= until
            {
                return Ok(entry);
            }
            if tokio::time::timeout_at(until, changed.changed())
                .await
                .is_err()
            {
                return self.gateway.get(caller, id);
            }
        }
    }

    async fn gateway_queue(self: &Arc<Self>, caller: &Caller, request: Queue) -> Result<Value> {
        validate_input(&request.call_ref, &request.prompt)?;
        let until = deadline(request.timeout_ms)?;
        let previous = self.gateway.get(caller, &request.work_id)?;
        if previous.spec.call_ref == request.call_ref {
            return Err(err("idempotency_conflict", "queue requires a new call_ref"));
        }
        let start = || Start {
            provider: previous.spec.resolved.provider.name().into(),
            cwd: previous.spec.cwd.clone(),
            call_ref: request.call_ref.clone(),
            prompt: request.prompt.clone(),
            title: None,
            model: previous.spec.resolved.model.clone(),
            effort: previous.effort.clone(),
            write: previous.write,
            continues: Some(request.work_id.clone()),
            thread: None,
        };
        loop {
            match {
                let _work = self.enter_gateway()?;
                self.gateway_start(caller, start())
            } {
                Err(e)
                    if e.downcast_ref::<GatewayError>()
                        .is_some_and(|e| e.code == "thread_busy") => {}
                result => return result,
            }
            let parent = self.gateway.get(caller, &request.work_id)?;
            let current = match &parent.thread {
                Some(thread) => self.gateway.by_thread(caller, thread)?,
                None => parent,
            };
            if matches!(current.record.state, WorkState::Unverified { .. })
                || waiting_for_input(&current.record)
                || (current.record.state.finished() && current.thread.is_none())
            {
                return Err(err(
                    "thread_unavailable",
                    "previous work has no confirmed reusable thread",
                ));
            }
            if Instant::now() >= until {
                return Err(err(
                    "timeout",
                    "queue wait expired before continuation admission",
                ));
            }
            self.gateway_wait(caller, &current.spec.work_id, until)
                .await?;
            if Instant::now() >= until {
                return Err(err(
                    "timeout",
                    "queue wait expired before continuation admission",
                ));
            }
        }
    }

    async fn gateway_stop(&self, caller: &Caller, request: Work) -> Result<Value> {
        let entry = self.gateway.get(caller, &request.work_id)?;
        if !matches!(
            entry.record.state,
            WorkState::Running { .. } | WorkState::Interrupting { .. }
        ) {
            return Err(err("not_running", "work has no confirmed running turn"));
        }
        if self
            .gateway
            .claim_control(caller, &request.work_id, "stop", "stop", "")?
        {
            self.gateway
                .signal(&request.work_id, Signal::InterruptRequested)?;
            if self
                .work_executor(&entry.record)?
                .interrupt(&entry.record)
                .await
                .is_err()
            {
                self.gateway
                    .signal(&request.work_id, Signal::InterruptTimedOut)?;
                return Err(err(
                    "submission_unverified",
                    "executor did not confirm the stop",
                ));
            }
        }
        Ok(self.gateway.get(caller, &request.work_id)?.compact())
    }

    async fn gateway_steer(&self, caller: &Caller, request: Steer) -> Result<Value> {
        validate_input(&request.call_ref, &request.prompt)?;
        let entry = self.gateway.get(caller, &request.work_id)?;
        let WorkState::Running { handle } = &entry.record.state else {
            return Err(err("not_running", "work has no confirmed running turn"));
        };
        let key = format!("steer:{}", request.call_ref);
        let into = SteerTarget {
            provider: entry.spec.resolved.provider,
            attempt_id: entry.spec.work_id.clone(),
            thread_id: handle.thread_id.clone(),
            turn_id: handle.turn_id.clone(),
        };
        let executor = self.work_executor(&entry.record)?;
        let message_id = WorkSpec::id(&entry.spec.conversation, &request.work_id, &key);
        if self
            .gateway
            .claim_control(caller, &request.work_id, &key, "steer", &request.prompt)?
        {
            let input = SteerInput {
                text: request.prompt,
                files: vec![],
            };
            match executor.send_turn(&into, &message_id, &input).await {
                SteerAck::Accepted { turn_id } => {
                    return Ok(
                        json!({"work_id":request.work_id,"accepted":true,"turn_id":turn_id}),
                    );
                }
                SteerAck::Refused(reason) => return Err(err("steer_refused", &reason)),
                SteerAck::Unknown => {}
            }
        }
        match executor.read_back(&into, &message_id).await {
            Some(SteerTrace::Received { turn_id } | SteerTrace::Ended { turn_id, .. }) => {
                Ok(json!({"work_id":request.work_id,"accepted":true,"turn_id":turn_id}))
            }
            Some(SteerTrace::NotArrived) => {
                Err(err("steer_refused", "steer did not reach the active turn"))
            }
            _ => Err(err("submission_unverified", "steer has not been confirmed")),
        }
    }

    pub(super) fn start_gateway_workers(self: &Arc<Self>) -> Result<()> {
        let node = Arc::downgrade(self);
        self.spawn_background(async move {
            while let Some(node) = node.upgrade() {
                if let Err(error) = node.gateway.deliver().await {
                    tracing::warn!(%error,"gateway outbox delivery pending");
                }
                drop(node);
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
        })?;
        let node = Arc::downgrade(self);
        self.spawn_background(async move {
            use futures_util::{StreamExt, stream};
            while let Some(node) = node.upgrade() {
                match node.gateway.pending() {
                    Ok(records) => {
                        stream::iter(records)
                            .for_each_concurrent(8, |work| async {
                                if let Err(error) = node.gateway_probe(work).await {
                                    tracing::debug!(%error,"gateway work reconcile pending");
                                }
                            })
                            .await
                    }
                    Err(error) => tracing::warn!(%error,"gateway journal unavailable"),
                }
                drop(node);
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
        })
    }

    async fn gateway_probe(&self, record: WorkRecord) -> Result<()> {
        let mut details = None;
        let signal = if record.state.thread().is_none() {
            if !matches!(record.state, WorkState::Accepted) || !record.send_window_over(now_ms()) {
                return Ok(());
            }
            Signal::StartRejected(ExecFailure::new(
                FailureKind::Rejected,
                "no native thread was recorded in the start window",
            ))
        } else {
            let read = if record.kind == WorkKind::Agent(Provider::Codex) {
                let thread = record.state.thread().expect("journaled thread");
                self.codex
                    .probe_with(
                        thread,
                        record.state.handle().map(|h| h.turn_id.as_str()),
                        &record.work_id,
                        |turn| details = Some(TurnDetails::codex(turn)),
                    )
                    .await
                    .map_err(anyhow::Error::from)
            } else {
                self.work_executor(&record)?.probe(&record).await
            };
            let probe = match read {
                Ok(probe) => probe,
                Err(_) if record.send_window_over(now_ms()) => {
                    self.gateway.signal(
                        &record.work_id,
                        Signal::SubmissionUnverified(
                            "executor is unavailable; awaiting readback".into(),
                        ),
                    )?;
                    return Ok(());
                }
                Err(e) => return Err(e),
            };
            if matches!(probe, Probe::NotStarted | Probe::Missing)
                && !record.send_window_over(now_ms())
            {
                return Ok(());
            }
            if matches!(probe, Probe::Running { .. })
                && matches!(
                    record.state,
                    WorkState::Running { .. } | WorkState::Interrupting { .. }
                )
            {
                return Ok(());
            }
            Signal::Probed(probe)
        };
        let current = self
            .gateway
            .signal_with_details(&record.work_id, signal, details)?;
        if current.kind == WorkKind::Agent(Provider::Codex) && current.state.finished() {
            self.codex.release_work(&current.work_id);
        }
        Ok(())
    }
}
