//! Triage workers (design 4): capture into one queue, hand batches to the coordination Agent,
//! send what it raises to the outlets, deliver webhooks from a local outbox, hold App Server
//! requests for the Agent, and accept decisions for local sessions.
use super::{Access, Runtime, now_ms};
use crate::{
    config,
    manager::{self, ManagerConfig, ManagerKind},
    model::{ATTENTION_SCOPE, AlertResolution, EventBody},
    net,
    store::{local_load, local_prefix, local_save},
    triage::{self, AgentEvent, Card, Delivery, Matter, OutboxEntry, Severity, TriageConfig},
};
use anyhow::{Context as _, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

const WAKE_EVERY: Duration = Duration::from_secs(20);
pub const TRIAGE_URL_ENV: &str = "VIBEFACE_TRIAGE_URL";
const BATCH: usize = 20;
const SCAN_EVERY: Duration = Duration::from_secs(300);
const SCAN_WINDOW_MS: i64 = 24 * 3600 * 1000;
const SEEN_TTL_MS: i64 = 7 * 24 * 3600 * 1000;
/// How long an App Server request waits for the coordination Agent before the fallback answer.
pub(crate) const AGENT_ANSWER_WITHIN: Duration = Duration::from_secs(120);
const WEBHOOK_KEY: &str = "triage-webhook.key";
const DECISION_TOKENS: &str = "decision-tokens.json";
pub const SPOOL_DIR: &str = "hook-spool";

pub(crate) struct TriageState {
    config: std::sync::Mutex<TriageConfig>,
    /// App Server requests waiting for an answer, by the Node's own request ID. They live only as
    /// long as the connection that carried them.
    requests: std::sync::Mutex<HashMap<String, CodexRequest>>,
    /// Answers given in this process: answering again returns the first answer.
    answered: std::sync::Mutex<HashMap<String, Value>>,
    wake: tokio::sync::Notify,
    outbox: tokio::sync::Notify,
    /// App Server threads' working directory and last answer, for their turn-end events.
    threads: std::sync::Mutex<HashMap<String, (String, String)>>,
    started_ms: i64,
}

impl TriageState {
    pub(crate) fn new(config: TriageConfig) -> Self {
        Self {
            config: std::sync::Mutex::new(config),
            requests: Default::default(),
            answered: Default::default(),
            wake: Default::default(),
            outbox: Default::default(),
            threads: Default::default(),
            started_ms: now_ms(),
        }
    }
    fn config(&self) -> TriageConfig {
        self.config
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
}

#[derive(Clone, Serialize)]
struct CodexRequest {
    request_id: String,
    #[serde(skip)]
    rpc_id: Value,
    method: String,
    thread_id: Option<String>,
    turn_id: Option<String>,
    params: Value,
    received_ms: i64,
    /// `None` while a decision card holds it for the user.
    answer_by_ms: Option<i64>,
    alert_id: Option<String>,
}

fn lock<T>(mutex: &std::sync::Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// What a request asks, for the event the Agent and the outlets see. Commands, patches and
/// paths are left out; the Agent reads them with `vibeface codex requests`.
fn describe(method: &str, params: &Value) -> String {
    let questions: Vec<&str> = params["questions"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|question| question["question"].as_str())
        .collect();
    json!({"method":method,"reason":params["reason"],"message":params["message"],
        "questions":questions,"server":params["serverName"]})
    .to_string()
}

#[derive(Serialize, Deserialize)]
struct Seen {
    event_id: String,
    ts_ms: i64,
}

#[derive(Serialize, Deserialize)]
struct Handled {
    event_id: String,
    ts_ms: i64,
    result: Value,
}

#[derive(Serialize, Deserialize)]
struct ScanMark {
    offset: usize,
}

#[derive(Clone, Serialize, Deserialize)]
struct DecisionToken {
    token_id: String,
    hash: String,
    label: String,
    created_ms: i64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DecisionInput {
    device_id: String,
    thread_id: String,
    #[serde(default)]
    text: String,
    decision_id: String,
    #[serde(default)]
    alert_id: Option<String>,
    #[serde(default)]
    choice: Option<usize>,
}

fn queue_key(event: &AgentEvent) -> String {
    format!("triage.queue.{:013}.{}", event.ts_ms.max(0), event.event_id)
}

fn delete(connection: &rusqlite::Connection, network: &str, key: &str) -> Result<()> {
    connection.execute(
        "DELETE FROM local_values WHERE network_id=? AND key=?",
        rusqlite::params![network, key],
    )?;
    Ok(())
}

fn read_private(path: &Path) -> Result<Option<Vec<u8>>> {
    match std::fs::read(path) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

impl Runtime {
    async fn network_id(&self) -> Result<String> {
        Ok(self.membership_state().await?.genesis.network_id)
    }

    /// Takes one event from a hook, a scan or the App Server. Test events are acknowledged and
    /// dropped; so is everything while no coordination Agent is configured to judge it. An event
    /// ID seen before is a duplicate, whichever source sent it first.
    pub(crate) async fn ingest_agent_event(&self, event: AgentEvent) -> Result<Value> {
        self.queue_event(AgentEvent {
            request_id: None,
            ..event
        })
        .await
    }

    async fn queue_event(&self, event: AgentEvent) -> Result<Value> {
        let event = event.seal(&self.identity.device_id()?, &config::hostname(), now_ms())?;
        if event.test {
            tracing::info!(event_id = event.event_id, "triage test event received");
            return Ok(json!({"status":"accepted","event_id":event.event_id,"test":true}));
        }
        let network = self.network_id().await?;
        if self.settings_snapshot().manager.is_none() {
            return Ok(json!({"status":"dropped","event_id":event.event_id}));
        }
        let store = self.store.lock().await;
        let seen = format!("triage.seen.{}", event.event_id);
        if local_load::<Seen>(&store.connection, &network, &seen)?.is_some() {
            return Ok(json!({"status":"duplicate","event_id":event.event_id}));
        }
        local_save(
            &store.connection,
            &network,
            &seen,
            &Seen {
                event_id: event.event_id.clone(),
                ts_ms: now_ms(),
            },
        )?;
        local_save(&store.connection, &network, &queue_key(&event), &event)?;
        Ok(json!({"status":"accepted","event_id":event.event_id}))
    }

    /// A server request the bridge handed over: held for the coordination Agent, or answered by
    /// the fallback at once when there is none.
    pub(crate) async fn on_codex_request(self: &Arc<Self>, params: &Value) {
        let request = CodexRequest {
            request_id: uuid::Uuid::new_v4().simple().to_string(),
            rpc_id: params["id"].clone(),
            method: params["method"].as_str().unwrap_or_default().into(),
            thread_id: params["threadId"].as_str().map(str::to_owned),
            turn_id: params["params"]["turnId"].as_str().map(str::to_owned),
            params: params["params"].clone(),
            received_ms: now_ms(),
            answer_by_ms: Some(now_ms() + AGENT_ANSWER_WITHIN.as_millis() as i64),
            alert_id: None,
        };
        tracing::info!(request = %request.request_id, method = %request.method,
            thread = ?request.thread_id, "App Server request received");
        let request_id = request.request_id.clone();
        lock(&self.triage.requests).insert(request_id.clone(), request.clone());
        let cwd = request
            .thread_id
            .as_ref()
            .and_then(|thread| lock(&self.triage.threads).get(thread).map(|t| t.0.clone()))
            .unwrap_or_default();
        let event = AgentEvent {
            source: triage::Source::AppServer,
            agent: triage::Agent::Codex,
            event: triage::Kind::NeedsInput,
            session_id: request.thread_id.clone().unwrap_or("app-server".into()),
            turn_id: format!("request:{request_id}"),
            cwd,
            detail: describe(&request.method, &request.params),
            request_id: Some(request_id.clone()),
            ..Default::default()
        };
        let queued = self.queue_event(event).await;
        if !matches!(&queued, Ok(result) if result["status"] == "accepted") {
            if let Err(error) = &queued {
                tracing::warn!(%error, "App Server request could not reach the Agent");
            }
            let _ = self
                .answer_codex_request(&request_id, None, "fallback")
                .await;
            return;
        }
        self.triage.wake.notify_one();
    }

    /// Answers a held request once: `result` from the Agent or the user, or the method's fallback
    /// when `None`. Answering again returns the first answer.
    pub(crate) async fn answer_codex_request(
        &self,
        request_id: &str,
        result: Option<Result<Value, Value>>,
        by: &str,
    ) -> Result<Value> {
        if let Some(done) = lock(&self.triage.answered).get(request_id) {
            return Ok(done.clone());
        }
        let request = lock(&self.triage.requests)
            .remove(request_id)
            .context("no App Server request is waiting with this ID")?;
        let (result, decision) = match result {
            Some(result) => {
                let decision = result
                    .as_ref()
                    .ok()
                    .and_then(|r| r["decision"].as_str().or(r["action"].as_str()))
                    .unwrap_or(if result.is_ok() { "answered" } else { "error" })
                    .to_owned();
                (result, decision)
            }
            None => {
                let (result, decision) = vibeface_bridge::methods::fallback(&request.method);
                (result, decision.to_owned())
            }
        };
        let sent = self
            .codex
            .client()
            .respond(&request.rpc_id, result, &decision)
            .await;
        let outcome = json!({"request_id":request_id,"method":request.method,"thread_id":request.thread_id,
            "turn_id":request.turn_id,"decision":decision,"by":by,"answered":sent.is_ok(),
            "error":sent.as_ref().err().map(|error| error.message.clone())});
        tracing::info!(request = %request_id, method = %request.method, %decision, %by,
            answered = sent.is_ok(), "App Server request answered");
        {
            let mut answered = lock(&self.triage.answered);
            // ponytail: forget answers past 1024; a restart forgets them all, as the requests.
            if answered.len() > 1024 {
                answered.clear();
            }
            answered.insert(request_id.into(), outcome.clone());
        }
        if let Ok(network) = self.network_id().await {
            let store = self.store.lock().await;
            for event in local_prefix::<AgentEvent>(&store.connection, &network, "triage.queue.")? {
                if event.request_id.as_deref() == Some(request_id) {
                    delete(&store.connection, &network, &queue_key(&event))?;
                }
            }
        }
        Ok(outcome)
    }

    /// Requests past their deadline, or whose card the user dismissed, get the fallback answer.
    async fn expire_codex_requests(&self) -> Result<()> {
        let now = now_ms();
        let requests: Vec<CodexRequest> = lock(&self.triage.requests).values().cloned().collect();
        let network = self.network_id().await.ok();
        for request in requests {
            let dismissed = match (&request.alert_id, &network) {
                (Some(alert_id), Some(network)) => self
                    .store
                    .lock()
                    .await
                    .connection
                    .query_row(
                        "SELECT resolution FROM alert_resolutions WHERE network_id=? AND alert_id=?",
                        rusqlite::params![network, alert_id],
                        |row| row.get::<_, String>(0),
                    )
                    .is_ok_and(|resolution| resolution == "dismissed"),
                _ => false,
            };
            if dismissed || request.answer_by_ms.is_some_and(|by| by <= now) {
                self.answer_codex_request(&request.request_id, None, "fallback")
                    .await?;
            }
        }
        Ok(())
    }

    /// The choices and request a card was raised with; they stay on this Node.
    pub(crate) async fn decision_card(
        &self,
        network: &str,
        alert_id: &str,
    ) -> Result<Option<Card>> {
        let store = self.store.lock().await;
        local_load(
            &store.connection,
            network,
            &format!("triage.card.{alert_id}"),
        )
    }

    /// The connection that carried the held requests is gone, and so are they.
    pub(crate) fn drop_codex_requests(&self) {
        for (request_id, request) in lock(&self.triage.requests).drain() {
            tracing::info!(request = %request_id, method = %request.method,
                "App Server request lost with its connection");
        }
    }

    /// Turn ends of every thread the managed App Server reports, not only the Node's own work.
    pub(crate) fn triage_app_server(self: &Arc<Self>, event: &Value) {
        let params = &event["params"];
        let thread = params["threadId"]
            .as_str()
            .or_else(|| params["thread"]["id"].as_str())
            .unwrap_or_default()
            .to_owned();
        if thread.is_empty() {
            return;
        }
        let mut threads = self
            .triage
            .threads
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // ponytail: forget everything past 1024 threads; an LRU if long-lived Nodes need it.
        if threads.len() > 1024 {
            threads.clear();
        }
        let entry = threads.entry(thread.clone()).or_default();
        match event["method"].as_str().unwrap_or_default() {
            "thread/started" => {
                entry.0 = params["thread"]["cwd"].as_str().unwrap_or_default().into();
            }
            "item/completed" if params["item"]["type"] == "agentMessage" => {
                entry.1 = params["item"]["text"].as_str().unwrap_or_default().into();
            }
            "turn/completed" => {
                let status = params["turn"]["status"].as_str().unwrap_or("completed");
                let event = AgentEvent {
                    source: triage::Source::AppServer,
                    agent: triage::Agent::Codex,
                    event: triage::Kind::TurnEnd,
                    status: Some(status.into()),
                    session_id: thread,
                    turn_id: params["turn"]["id"].as_str().unwrap_or_default().into(),
                    cwd: entry.0.clone(),
                    last_message: std::mem::take(&mut entry.1),
                    detail: params["turn"]["error"]["message"]
                        .as_str()
                        .unwrap_or_default()
                        .into(),
                    ..Default::default()
                };
                let node = self.clone();
                tokio::spawn(async move {
                    if let Err(error) = node.ingest_agent_event(event).await {
                        tracing::debug!(%error, "App Server turn was not triaged");
                    }
                });
            }
            _ => {}
        }
    }

    pub(super) fn start_triage(self: &Arc<Self>) {
        let node = self.clone();
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = tokio::time::sleep(WAKE_EVERY) => {}
                    _ = node.triage.wake.notified() => {}
                }
                if let Err(error) = node.wake_agent().await {
                    tracing::warn!(%error, "could not hand triage events to the Agent");
                }
            }
        });
        let node = self.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(5)).await;
                if let Err(error) = node.expire_codex_requests().await {
                    tracing::warn!(%error, "could not answer expired App Server requests");
                }
            }
        });
        let node = self.clone();
        tokio::spawn(async move {
            loop {
                if let Err(error) = node.deliver_webhooks().await {
                    tracing::warn!(%error, "webhook delivery failed");
                }
                let _ = tokio::time::timeout(Duration::from_secs(5), node.triage.outbox.notified())
                    .await;
            }
        });
        let node = self.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(5)).await;
            loop {
                if let Err(error) = node.scan_sources().await {
                    tracing::warn!(%error, "session scan failed");
                }
                tokio::time::sleep(SCAN_EVERY).await;
            }
        });
    }

    async fn pending_events(&self, network: &str) -> Result<Vec<AgentEvent>> {
        let store = self.store.lock().await;
        local_prefix(&store.connection, network, "triage.queue.")
    }

    /// Hands waiting events to one turn of the coordination Agent. It answers through
    /// `vibeface triage` and `vibeface codex respond`; what it leaves alone is dropped after the
    /// turn, and a request it neither answered nor carded gets the fallback answer.
    async fn wake_agent(&self) -> Result<()> {
        let Some(settings) = self.settings_snapshot().manager else {
            return Ok(());
        };
        let network = self.network_id().await?;
        let fresh: Vec<AgentEvent> = self
            .pending_events(&network)
            .await?
            .into_iter()
            .take(BATCH)
            .collect();
        if fresh.is_empty() {
            return Ok(());
        }
        let program = std::env::current_exe()?;
        let cli = format!("'{}'", program.to_string_lossy().replace('\'', r"'\''"));
        let config = ManagerConfig {
            kind: match settings.provider.as_str() {
                "codex_sdk" => ManagerKind::CodexSdk,
                _ => ManagerKind::CursorSdk,
            },
            workspace: settings.workspace,
            model: settings.model,
            timeout_secs: self.config.timeout_secs,
            cursor_sdk_bin: settings.cursor_sdk_bin,
            node_bin: settings.node_bin,
            codex_bin: self.config.codex_bin.clone(),
            codex_sdk_script: settings.codex_sdk_script,
            tool_cli: Some(program),
            tool_env: [
                (
                    TRIAGE_URL_ENV.to_owned(),
                    format!("http://{}", self.config.local_listen),
                ),
                (
                    "VIBEFACE_CODEX_SOCKET".to_owned(),
                    self.codex.socket.display().to_string(),
                ),
            ]
            .into(),
            ..Default::default()
        };
        let (_cancel, cancel) = tokio::sync::watch::channel(false);
        let turn = manager::run_manager(
            &config,
            &triage::wake_prompt(&fresh, &cli, AGENT_ANSWER_WITHIN.as_secs())?,
            None,
            |_| async { Ok(()) },
            cancel,
            |_| async { Ok(()) },
        )
        .await;
        self.settle_offered(&network, &fresh).await?;
        turn.map(|_| ())
    }

    async fn settle_offered(&self, network: &str, offered: &[AgentEvent]) -> Result<()> {
        for event in offered {
            if let Some(request_id) = &event.request_id {
                let unattended = lock(&self.triage.requests)
                    .get(request_id)
                    .is_some_and(|request| request.answer_by_ms.is_some());
                if unattended {
                    self.answer_codex_request(request_id, None, "fallback")
                        .await?;
                }
            }
            let store = self.store.lock().await;
            delete(&store.connection, network, &queue_key(event))?;
        }
        Ok(())
    }

    /// `/api/triage`, the coordination Agent's side: `events`, `raise` and `dismiss`.
    pub(crate) async fn triage_command(&self, input: Value) -> Result<Value> {
        let network = self.network_id().await?;
        let action = super::field(&input, "action")?;
        if action == "requests" {
            let requests: Vec<CodexRequest> =
                lock(&self.triage.requests).values().cloned().collect();
            return Ok(json!({"requests":requests}));
        }
        if action == "respond" {
            let request_id = super::field(&input, "request_id")?;
            let result = input.get("result").cloned().context("result is required")?;
            return self
                .answer_codex_request(request_id, Some(Ok(result)), "agent")
                .await;
        }
        if action == "events" {
            let since = input["since"].as_i64().unwrap_or(0);
            let events: Vec<_> = self
                .pending_events(&network)
                .await?
                .into_iter()
                .filter(|event| event.ts_ms >= since)
                .collect();
            return Ok(json!({"events":events}));
        }
        let event_id = super::id(&input, "event_id")?;
        let handled_key = format!("triage.handled.{event_id}");
        {
            let store = self.store.lock().await;
            if let Some(handled) = local_load::<Handled>(&store.connection, &network, &handled_key)?
            {
                return Ok(handled.result);
            }
        }
        let event = self
            .pending_events(&network)
            .await?
            .into_iter()
            .find(|event| event.event_id == event_id)
            .context("no pending event with this ID")?;
        let result = match action {
            "dismiss" => json!({"event_id":event_id,"dismissed":true}),
            "raise" => {
                let severity: Severity = serde_json::from_value(input["severity"].clone())
                    .context("severity must be info, attention or urgent")?;
                let summary = super::field(&input, "summary")?;
                let matter = super::field(&input, "matter")?;
                ensure!(
                    summary.len() <= 1024 && matter.len() <= 256,
                    "summary or matter is too long"
                );
                let choices: Vec<triage::Choice> =
                    serde_json::from_value(input.get("choices").cloned().unwrap_or(json!([])))
                        .context("choices must be [{label, reply}]")?;
                ensure!(choices.len() <= 8, "at most 8 choices");
                let request_id = input["request_id"].as_str().map(str::to_owned);
                if let Some(request_id) = &request_id {
                    ensure!(
                        !choices.is_empty(),
                        "a card that holds a request needs choices"
                    );
                    ensure!(
                        lock(&self.triage.requests).contains_key(request_id),
                        "no App Server request is waiting with this ID"
                    );
                }
                let thread_id = input["thread_id"].as_str().map(str::to_owned).or_else(|| {
                    (event.agent == triage::Agent::Codex && event.session_id != "app-server")
                        .then(|| event.session_id.clone())
                });
                let card = Card {
                    thread_id,
                    request_id,
                    choices,
                };
                self.raise(&network, &event, severity, summary, matter, card)
                    .await?
            }
            _ => bail!("unknown triage action"),
        };
        let store = self.store.lock().await;
        local_save(
            &store.connection,
            &network,
            &handled_key,
            &Handled {
                event_id: event_id.clone(),
                ts_ms: now_ms(),
                result: result.clone(),
            },
        )?;
        delete(&store.connection, &network, &queue_key(&event))?;
        Ok(result)
    }

    /// Deterministic dedupe, not judgment: the same matter within the window updates its count
    /// and goes out again only when the severity rises.
    async fn raise(
        &self,
        network: &str,
        event: &AgentEvent,
        severity: Severity,
        summary: &str,
        matter: &str,
        card: Card,
    ) -> Result<Value> {
        let key = triage::matter_key(&event.node, matter);
        let matter_key = format!("triage.matter.{key}");
        let (matter, emit) = {
            let store = self.store.lock().await;
            let previous: Option<Matter> = local_load(&store.connection, network, &matter_key)?;
            // A card that holds a request always goes out on its own: the user must answer it.
            let previous = previous.filter(|_| card.request_id.is_none());
            let (matter, emit) =
                triage::merge(previous.as_ref(), &key, &event.event_id, severity, now_ms());
            local_save(&store.connection, network, &matter_key, &matter)?;
            if emit {
                local_save(
                    &store.connection,
                    network,
                    &format!("triage.card.{}", matter.alert_id),
                    &card,
                )?;
            }
            (matter, emit)
        };
        if emit && let Some(request_id) = &card.request_id {
            let mut requests = lock(&self.triage.requests);
            let request = requests
                .get_mut(request_id)
                .context("the App Server request was answered meanwhile")?;
            request.answer_by_ms = None;
            request.alert_id = Some(matter.alert_id.clone());
        }
        let labels: Vec<_> = card
            .choices
            .iter()
            .map(|choice| crate::model::ChoiceLabel {
                label: choice.label.clone(),
            })
            .collect();
        let config = self.triage.config();
        let outlet = config.outlet_for(&event.cwd);
        let mut result = json!({"event_id":event.event_id,"alert_id":matter.alert_id,"matter":key,
            "severity":matter.severity,"count":matter.count,"sent":emit,"outlet":outlet,
            "recorded":false,"queued_webhook":false});
        if !emit {
            return Ok(result);
        }
        if outlet.record() {
            self.append(
                network,
                ATTENTION_SCOPE,
                EventBody::AttentionRaised {
                    alert_id: matter.alert_id.clone(),
                    matter: key.clone(),
                    agent: serde_json::to_value(event.agent)?
                        .as_str()
                        .unwrap_or_default()
                        .into(),
                    session_id: event.session_id.clone(),
                    turn_id: (!event.turn_id.is_empty()).then(|| event.turn_id.clone()),
                    cwd: event.cwd.clone(),
                    severity: matter.severity,
                    summary: summary.into(),
                    count: matter.count,
                    thread_id: card.thread_id.clone(),
                    request_id: card.request_id.clone(),
                    choices: labels,
                },
            )
            .await?;
            result["recorded"] = json!(true);
        }
        if outlet.webhook() && config.webhook_url.is_some() {
            let body = json!({
                "schema_version":triage::SCHEMA_VERSION, "event_id":event.event_id,
                "alert_id":matter.alert_id, "matter":key, "severity":matter.severity,
                "summary":summary, "count":matter.count, "event":event,
                "request_id":card.request_id,
                "choices":card.choices.iter().enumerate()
                    .map(|(index, choice)| json!({"index":index,"label":choice.label}))
                    .collect::<Vec<_>>(),
                "decision":card.thread_id.as_ref().map(|thread| json!({
                    "url":self.decision_url(), "device_id":event.node, "thread_id":thread,
                    "alert_id":matter.alert_id})),
            });
            let store = self.store.lock().await;
            let outbox_key = format!("triage.outbox.{}", event.event_id);
            if local_load::<OutboxEntry>(&store.connection, network, &outbox_key)?.is_none() {
                local_save(
                    &store.connection,
                    network,
                    &outbox_key,
                    &OutboxEntry::new(&event.event_id, body.to_string(), now_ms()),
                )?;
            }
            drop(store);
            self.triage.outbox.notify_one();
            result["queued_webhook"] = json!(true);
        }
        Ok(result)
    }

    fn decision_url(&self) -> String {
        let base = self
            .config
            .network
            .entry
            .as_ref()
            .map_or(self.config.public_url.as_str(), |entry| entry.url.as_str());
        format!("{}/api/decision", base.trim_end_matches('/'))
    }

    fn webhook_secret(&self) -> Result<Option<Vec<u8>>> {
        read_private(&self.data_dir.join(WEBHOOK_KEY))
    }

    async fn deliver_webhooks(&self) -> Result<()> {
        let Some(url) = self.triage.config().webhook_url else {
            return Ok(());
        };
        let Some(secret) = self.webhook_secret()? else {
            return Ok(());
        };
        let network = self.network_id().await?;
        let entries: Vec<OutboxEntry> = {
            let store = self.store.lock().await;
            local_prefix(&store.connection, &network, "triage.outbox.")?
        };
        for mut entry in entries {
            if entry.state != Delivery::Pending || entry.next_ms > now_ms() {
                continue;
            }
            let (status, error) = post_webhook(&url, &secret, &entry.event_id, &entry.body).await;
            entry.record(status, error, now_ms());
            if entry.state == Delivery::Dead {
                tracing::warn!(
                    event_id = entry.event_id,
                    error = entry.last_error,
                    "webhook delivery dead-lettered"
                );
            }
            let store = self.store.lock().await;
            local_save(
                &store.connection,
                &network,
                &format!("triage.outbox.{}", entry.event_id),
                &entry,
            )?;
        }
        Ok(())
    }

    async fn scan_sources(&self) -> Result<()> {
        let spool = self.data_dir.join(SPOOL_DIR);
        if let Ok(entries) = std::fs::read_dir(&spool) {
            for entry in entries.flatten() {
                let path = entry.path();
                let parsed = std::fs::read(&path)
                    .ok()
                    .and_then(|bytes| serde_json::from_slice::<AgentEvent>(&bytes).ok());
                if let Some(event) = parsed
                    && let Err(error) = self.ingest_agent_event(event).await
                {
                    tracing::debug!(%error, "spooled hook event was rejected");
                }
                let _ = std::fs::remove_file(path);
            }
        }
        let network = self.network_id().await?;
        {
            let store = self.store.lock().await;
            let cutoff = now_ms() - SEEN_TTL_MS;
            for seen in local_prefix::<Seen>(&store.connection, &network, "triage.seen.")? {
                if seen.ts_ms < cutoff {
                    delete(
                        &store.connection,
                        &network,
                        &format!("triage.seen.{}", seen.event_id),
                    )?;
                }
            }
            for handled in local_prefix::<Handled>(&store.connection, &network, "triage.handled.")?
            {
                if handled.ts_ms < cutoff {
                    delete(
                        &store.connection,
                        &network,
                        &format!("triage.handled.{}", handled.event_id),
                    )?;
                }
            }
        }
        if !self.triage.config().scan_codex_sessions {
            return Ok(());
        }
        let root = std::env::var_os("CODEX_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".codex")))
            .context("no home directory")?
            .join("sessions");
        for path in rollouts(&root, now_ms() - SCAN_WINDOW_MS) {
            let key = format!(
                "triage.scan.{}",
                crate::crypto::digest(path.to_string_lossy().as_bytes())
            );
            let mark: Option<ScanMark> = {
                let store = self.store.lock().await;
                local_load(&store.connection, &network, &key)?
            };
            let Ok(text) = std::fs::read_to_string(&path) else {
                continue;
            };
            let offset = match mark {
                Some(mark) => mark.offset,
                // History from before this Node started is not news.
                None if modified_ms(&path) < self.triage.started_ms => text.len(),
                None => 0,
            };
            let (events, end) = triage::scan_rollout(&text, offset);
            for event in events {
                if let Err(error) = self.ingest_agent_event(event).await {
                    tracing::debug!(%error, "scanned turn was not triaged");
                }
            }
            let store = self.store.lock().await;
            local_save(&store.connection, &network, &key, &ScanMark { offset: end })?;
        }
        Ok(())
    }

    // ---- settings (local operator RPCs) ----

    pub(crate) async fn triage_settings(&self) -> Result<Value> {
        let config = self.triage.config();
        let tokens: Vec<Value> = self
            .decision_tokens()?
            .into_iter()
            .map(|t| json!({"token_id":t.token_id,"label":t.label,"created_ms":t.created_ms}))
            .collect();
        let mut outbox = json!({"pending":0,"delivered":0,"dead":0});
        if let Ok(network) = self.network_id().await {
            let store = self.store.lock().await;
            for entry in local_prefix::<OutboxEntry>(&store.connection, &network, "triage.outbox.")?
            {
                let state = serde_json::to_value(entry.state)?;
                let state = state.as_str().unwrap_or_default();
                outbox[state] = json!(outbox[state].as_u64().unwrap_or(0) + 1);
            }
        }
        Ok(json!({
            "default_outlet":config.default_outlet, "projects":config.projects,
            "webhook_url":config.webhook_url, "scan_codex_sessions":config.scan_codex_sessions,
            "webhook_secret_set":self.webhook_secret()?.is_some(),
            "decision_url":self.decision_url(), "device_id":self.identity.device_id()?,
            "decision_tokens":tokens, "outbox":outbox,
        }))
    }

    /// Replaces the outlet settings. `webhook_secret`: a string sets it, `null` clears it,
    /// absent keeps it; it is never read back.
    pub(crate) async fn configure_triage(&self, mut input: Value) -> Result<Value> {
        let _save = self.config_save.lock().await;
        let secret = input
            .as_object_mut()
            .and_then(|o| o.remove("webhook_secret"));
        let triage: TriageConfig =
            serde_json::from_value(input).context("invalid triage settings")?;
        triage.validate()?;
        match secret {
            None => {}
            Some(Value::Null) => match std::fs::remove_file(self.data_dir.join(WEBHOOK_KEY)) {
                Err(error) if error.kind() != std::io::ErrorKind::NotFound => {
                    return Err(error.into());
                }
                _ => {}
            },
            Some(Value::String(secret)) => {
                ensure!(
                    (16..=512).contains(&secret.len()),
                    "the webhook secret must be 16 to 512 characters"
                );
                config::private_write(&self.data_dir.join(WEBHOOK_KEY), secret.as_bytes())?;
            }
            Some(_) => bail!("webhook_secret must be a string or null"),
        }
        let mut configuration = config::Config::load(&self.data_dir)?;
        configuration.triage = triage.clone();
        configuration.save(&self.data_dir)?;
        *self
            .triage
            .config
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = triage;
        self.triage.outbox.notify_one();
        drop(_save);
        self.triage_settings().await
    }

    /// Sends one signed test event straight to the webhook, outside the outbox.
    pub(crate) async fn test_webhook(&self) -> Result<Value> {
        let url = self
            .triage
            .config()
            .webhook_url
            .context("set a webhook URL first")?;
        let secret = self
            .webhook_secret()?
            .context("set a webhook secret first")?;
        let event_id = format!("test-{}", uuid::Uuid::new_v4().simple());
        let body = json!({"schema_version":triage::SCHEMA_VERSION,"event_id":event_id,"test":true,
            "summary":"vibeface webhook test","device_id":self.identity.device_id()?})
        .to_string();
        let (status, error) = post_webhook(&url, &secret, &event_id, &body).await;
        Ok(json!({"ok":matches!(status, Some(200..=299)),"status":status,"error":error}))
    }

    fn decision_tokens(&self) -> Result<Vec<DecisionToken>> {
        Ok(match read_private(&self.data_dir.join(DECISION_TOKENS))? {
            Some(bytes) => serde_json::from_slice(&bytes)?,
            None => vec![],
        })
    }

    pub(crate) async fn create_decision_token(&self, label: &str) -> Result<Value> {
        let _save = self.config_save.lock().await;
        ensure!(label.len() <= 64, "label too long");
        let mut tokens = self.decision_tokens()?;
        ensure!(tokens.len() < 16, "at most 16 decision tokens");
        let token = format!("vfd_{}", crate::crypto::random_hex(32));
        let entry = DecisionToken {
            token_id: crate::crypto::random_hex(8),
            hash: crate::crypto::digest(token.as_bytes()),
            label: label.into(),
            created_ms: now_ms(),
        };
        tokens.push(entry.clone());
        config::private_write(
            &self.data_dir.join(DECISION_TOKENS),
            &serde_json::to_vec(&tokens)?,
        )?;
        Ok(
            json!({"token_id":entry.token_id,"token":token,"label":entry.label,"created_ms":entry.created_ms}),
        )
    }

    pub(crate) async fn revoke_decision_token(&self, token_id: &str) -> Result<Value> {
        let _save = self.config_save.lock().await;
        let mut tokens = self.decision_tokens()?;
        let before = tokens.len();
        tokens.retain(|t| t.token_id != token_id);
        config::private_write(
            &self.data_dir.join(DECISION_TOKENS),
            &serde_json::to_vec(&tokens)?,
        )?;
        Ok(json!({"revoked":tokens.len() != before}))
    }

    fn decision_token_valid(&self, token: &str) -> bool {
        let hash = crate::crypto::digest(token.as_bytes());
        self.decision_tokens()
            .unwrap_or_default()
            .iter()
            .fold(false, |found, t| net::same_token(&t.hash, &hash) | found)
    }

    // ---- network RPCs ----

    pub(super) async fn alerts(&self, access: &Access) -> Result<Value> {
        Ok(json!({"alerts":self.store.lock().await.alerts(&access.network_id, 100)?}))
    }

    pub(super) async fn resolve_alert(&self, access: &Access, input: &Value) -> Result<Value> {
        ensure!(access.human, "only the user resolves alerts");
        let alert_id = super::id(input, "alert_id")?;
        let resolution: AlertResolution = serde_json::from_value(input["resolution"].clone())
            .context("resolution must be decided or dismissed")?;
        let event = self
            .append(
                &access.network_id,
                ATTENTION_SCOPE,
                EventBody::AttentionResolved {
                    alert_id,
                    resolution,
                    attestation: Some(super::attestation(input)?),
                },
            )
            .await?;
        Ok(json!({"event_hash":event.hash}))
    }

    /// `POST /api/decision`: a public entry forwards to the device; a member Node handles its
    /// own sessions and forwards the rest; the target alone checks the token.
    pub(crate) async fn decision_http(self: &Arc<Self>, token: &str, body: Value) -> Result<Value> {
        let input: DecisionInput =
            serde_json::from_value(body.clone()).context("invalid decision")?;
        net::endpoint_id(&input.device_id)?;
        let forwarded = json!({"token":token,"decision":body});
        if self.config.public_entry {
            crate::client::reach_through_entry(self, &input.device_id).await?;
        } else if input.device_id == self.identity.device_id()? {
            return self.local_decision(token, input).await;
        }
        self.net()?
            .request(
                &input.device_id,
                net::DECISION,
                &forwarded,
                Duration::from_secs(90),
            )
            .await
    }

    async fn local_decision(self: &Arc<Self>, token: &str, input: DecisionInput) -> Result<Value> {
        ensure!(self.decision_token_valid(token), "invalid decision token");
        ensure!(
            input.device_id == self.identity.device_id()?,
            "this decision is for another device"
        );
        super::decision::send_decision(
            self,
            super::decision::DecisionRequest {
                thread_id: input.thread_id,
                text: input.text,
                decision_id: input.decision_id,
                alert_id: input.alert_id,
                choice: input.choice,
                ..Default::default()
            },
        )
        .await
        .and_then(|outcome| Ok(serde_json::to_value(outcome)?))
    }

    pub(super) async fn serve_decisions(
        self: Arc<Self>,
        connection: iroh::endpoint::Connection,
    ) -> Result<()> {
        net::serve_streams(connection, move |request: Value| {
            let node = self.clone();
            async move {
                let token = request["token"].as_str().context("token is required")?;
                let input: DecisionInput = serde_json::from_value(request["decision"].clone())
                    .context("invalid decision")?;
                node.local_decision(token, input).await
            }
        })
        .await;
        Ok(())
    }
}

/// One signed POST; the status, or the transport error.
async fn post_webhook(
    url: &str,
    secret: &[u8],
    event_id: &str,
    body: &str,
) -> (Option<u16>, Option<String>) {
    let timestamp = now_ms() / 1000;
    let client = match reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .redirect(reqwest::redirect::Policy::none())
        .build()
    {
        Ok(client) => client,
        Err(error) => return (None, Some(error.to_string())),
    };
    match client
        .post(url)
        .header("content-type", "application/json")
        .header("x-vibeface-event-id", event_id)
        .header("x-vibeface-timestamp", timestamp.to_string())
        .header(
            "x-vibeface-signature",
            triage::signature(secret, timestamp, body),
        )
        .body(body.to_owned())
        .send()
        .await
    {
        Ok(response) => (Some(response.status().as_u16()), None),
        // Without the URL: it may carry a path the user treats as private.
        Err(error) => (None, Some(error.without_url().to_string())),
    }
}

fn modified_ms(path: &Path) -> i64 {
    std::fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_millis() as i64)
}

/// `rollout-*.jsonl` under `root` modified after `since_ms`.
fn rollouts(root: &Path, since_ms: i64) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let mut stack = vec![root.to_owned()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            if kind.is_dir() {
                stack.push(path);
            } else if kind.is_file()
                && path.file_name().is_some_and(|n| {
                    let n = n.to_string_lossy();
                    n.starts_with("rollout-") && n.ends_with(".jsonl")
                })
                && modified_ms(&path) >= since_ms
            {
                found.push(path);
            }
        }
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        config::Config,
        crypto::LocalIdentity,
        identity::{Genesis, Member, NetworkState},
        model::NodeRole,
    };

    async fn node() -> Result<(Arc<Runtime>, LocalIdentity, PathBuf)> {
        let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../.artifacts")
            .join(format!("triage-{}", uuid::Uuid::new_v4()));
        config::private_dir(&dir)?;
        let mut configuration = Config::local("127.0.0.1:0".parse()?, vec![], false);
        configuration.roles = vec![NodeRole::Relay, NodeRole::Replica];
        // A unix socket path must stay short; nothing listens unless a test binds it.
        let socket = format!(
            "/tmp/vf-{}.sock",
            &uuid::Uuid::new_v4().simple().to_string()[..12]
        );
        configuration.codex_socket = Some(socket.into());
        configuration.save(&dir)?;
        config::private_write(&dir.join("data-epoch.json"), br#"{"data_epoch":7}"#)?;
        config::private_write(
            &dir.join("identity.json"),
            &serde_json::to_vec(&LocalIdentity::generate())?,
        )?;
        let node = Arc::new(Runtime::open(&dir).await?);
        let admin = LocalIdentity::generate();
        let state = NetworkState::new(Genesis::create(
            &LocalIdentity::generate(),
            Member::client(&admin, "admin", true)?,
            Member::node(&node.identity, "node")?,
        )?)?;
        node.install_membership(state).await?;
        Ok((node, admin, dir))
    }

    fn with_agent(node: &Runtime) {
        node.settings.lock().unwrap().manager = Some(crate::config::ManagerSettings {
            provider: "codex_sdk".into(),
            workspace: "/tmp".into(),
            model: "fixture".into(),
            cursor_sdk_bin: "/nonexistent".into(),
            node_bin: "/nonexistent".into(),
            codex_sdk_script: "/nonexistent".into(),
        });
    }

    type Server = tokio_tungstenite::WebSocketStream<tokio::net::UnixStream>;

    async fn read(server: &mut Server) -> Value {
        use futures_util::StreamExt as _;
        let message = server.next().await.unwrap().unwrap();
        serde_json::from_str(message.to_text().unwrap()).unwrap()
    }

    /// A fake App Server on the Node's socket that sends one approval request without a thread,
    /// which the bridge hands to the Node.
    async fn app_server(node: &Runtime) -> Result<(Server, Value)> {
        use futures_util::SinkExt as _;
        let listener = tokio::net::UnixListener::bind(&node.codex.socket)?;
        let (stream, _) = tokio::time::timeout(Duration::from_secs(5), listener.accept()).await??;
        let mut server = tokio_tungstenite::accept_async(stream).await?;
        let initialize = read(&mut server).await;
        let ready = json!({"id":initialize["id"],"result":{"userAgent":"codex/fixture"}});
        server.send(ready.to_string().into()).await?;
        assert_eq!(read(&mut server).await["method"], "initialized");
        let params = json!({"command":"rm -rf build","reason":"clean the build"});
        let request =
            json!({"id":41,"method":"item/commandExecution/requestApproval","params":params});
        server.send(request.to_string().into()).await?;
        while node.codex.client().snapshot().await["pending_requests"] == json!([]) {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let handed = json!({"id":41,"method":"item/commandExecution/requestApproval","threadId":null,"params":params});
        Ok((server, handed))
    }

    /// The answer the fake App Server got for request 41, and that nothing else follows it.
    async fn answered_once(server: &mut Server) -> Value {
        let answer = read(server).await;
        assert_eq!(answer["id"], 41);
        let again = tokio::time::timeout(Duration::from_millis(300), read(server)).await;
        assert!(again.is_err(), "the request is answered once");
        answer
    }

    #[tokio::test]
    async fn a_held_request_is_answered_by_the_agent_once() -> Result<()> {
        let (node, _, dir) = node().await?;
        let result = async {
            with_agent(&node);
            let (mut server, handed) = app_server(&node).await?;
            node.on_codex_request(&handed).await;
            let requests = node.triage_command(json!({"action":"requests"})).await?;
            let request_id = requests["requests"][0]["request_id"]
                .as_str()
                .unwrap()
                .to_owned();
            assert_eq!(
                requests["requests"][0]["method"],
                "item/commandExecution/requestApproval"
            );
            let events = node.triage_command(json!({"action":"events"})).await?;
            assert_eq!(events["events"][0]["request_id"], request_id.as_str());
            assert!(
                !events["events"][0]["detail"]
                    .as_str()
                    .unwrap()
                    .contains("rm -rf"),
                "commands stay out of events"
            );

            let respond =
                json!({"action":"respond","request_id":request_id,"result":{"decision":"accept"}});
            let answered = node.triage_command(respond.clone()).await?;
            assert_eq!(
                (answered["answered"].clone(), answered["by"].clone()),
                (json!(true), json!("agent"))
            );
            assert_eq!(
                answered_once(&mut server).await["result"],
                json!({"decision":"accept"})
            );
            assert_eq!(
                node.triage_command(respond).await?,
                answered,
                "answering again returns the first answer"
            );
            assert_eq!(
                node.triage_command(json!({"action":"requests"})).await?["requests"],
                json!([])
            );
            assert_eq!(
                node.triage_command(json!({"action":"events"})).await?["events"],
                json!([])
            );
            Ok::<_, anyhow::Error>(())
        }
        .await;
        let _ = std::fs::remove_file(&node.codex.socket);
        drop(node);
        std::fs::remove_dir_all(dir)?;
        result
    }

    #[tokio::test]
    async fn without_an_agent_requests_are_declined_and_events_dropped() -> Result<()> {
        let (node, _, dir) = node().await?;
        let result = async {
            let (mut server, handed) = app_server(&node).await?;
            node.on_codex_request(&handed).await;
            assert_eq!(
                answered_once(&mut server).await["result"],
                json!({"decision":"decline"})
            );
            assert_eq!(
                node.triage_command(json!({"action":"requests"})).await?["requests"],
                json!([])
            );
            let recorded = node
                .triage
                .answered
                .lock()
                .unwrap()
                .values()
                .next()
                .cloned()
                .unwrap();
            assert_eq!(
                (recorded["decision"].clone(), recorded["by"].clone()),
                (json!("decline"), json!("fallback"))
            );
            assert_eq!(
                node.ingest_agent_event(turn("s1", "t1", "/x")).await?["status"],
                "dropped"
            );
            assert_eq!(
                node.triage_command(json!({"action":"events"})).await?["events"],
                json!([])
            );
            Ok::<_, anyhow::Error>(())
        }
        .await;
        let _ = std::fs::remove_file(&node.codex.socket);
        drop(node);
        std::fs::remove_dir_all(dir)?;
        result
    }

    #[tokio::test]
    async fn a_choice_answers_the_carded_request_once() -> Result<()> {
        let (node, _, dir) = node().await?;
        let result = async {
            with_agent(&node);
            let (mut server, handed) = app_server(&node).await?;
            node.on_codex_request(&handed).await;
            let event = node.triage_command(json!({"action":"events"})).await?["events"][0].clone();
            let request_id = event["request_id"].as_str().unwrap().to_owned();
            let raised = node.triage_command(json!({"action":"raise","event_id":event["event_id"],"severity":"urgent",
                "summary":"要清理 build 吗","matter":"codex:clean","thread_id":"t","request_id":request_id,
                "choices":[{"label":"批准","reply":{"decision":"accept"}},{"label":"拒绝","reply":{"decision":"decline"}}]})).await?;
            assert_eq!(raised["sent"], true);
            let requests = node.triage_command(json!({"action":"requests"})).await?;
            assert_eq!(requests["requests"][0]["answer_by_ms"], Value::Null, "the card holds it for the user");

            let network = node.network_id().await?;
            let alert = node.store.lock().await.alerts(&network, 10)?[0].clone();
            assert_eq!((alert["thread_id"].clone(), alert["request_id"].clone()), (json!("t"), json!(request_id)));
            assert_eq!(alert["choices"], json!([{"label":"批准"},{"label":"拒绝"}]), "replies stay on the Node");
            let decision = super::super::decision::DecisionRequest {
                thread_id: "t".into(),
                decision_id: "d1".into(),
                alert_id: alert["alert_id"].as_str().map(str::to_owned),
                choice: Some(0),
                ..Default::default()
            };
            let sent = super::super::decision::send_decision(&node, decision.clone()).await?;
            assert!(matches!(&sent.delivery, vibeface_bridge::codex::Delivery::Delivered { mode, .. } if mode == "answer"));
            assert_eq!(answered_once(&mut server).await["result"], json!({"decision":"accept"}));
            let retried = super::super::decision::send_decision(&node, decision).await?;
            assert_eq!(retried, sent, "a retry with the same decision ID sends nothing");
            assert!(tokio::time::timeout(Duration::from_millis(300), read(&mut server)).await.is_err());
            assert_eq!(node.store.lock().await.alerts(&network, 10)?[0]["resolution"], "decided");
            Ok::<_, anyhow::Error>(())
        }
        .await;
        let _ = std::fs::remove_file(&node.codex.socket);
        drop(node);
        std::fs::remove_dir_all(dir)?;
        result
    }

    fn turn(session: &str, turn: &str, cwd: &str) -> AgentEvent {
        AgentEvent {
            source: triage::Source::CodexHook,
            session_id: session.into(),
            turn_id: turn.into(),
            cwd: cwd.into(),
            status: Some("failed".into()),
            last_message: "tests fail".into(),
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn events_dedupe_and_raises_route_once_to_cards_and_signed_webhooks() -> Result<()> {
        let (node, admin, dir) = node().await?;
        let received = Arc::new(std::sync::Mutex::new(
            Vec::<(axum::http::HeaderMap, String)>::new(),
        ));
        let sink = received.clone();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let url = format!("http://{}/hook", listener.local_addr()?);
        let server = tokio::spawn(async move {
            let app = axum::Router::new().route(
                "/hook",
                axum::routing::post(move |headers: axum::http::HeaderMap, body: String| {
                    let sink = sink.clone();
                    async move {
                        sink.lock().unwrap().push((headers, body));
                        axum::http::StatusCode::ACCEPTED
                    }
                }),
            );
            axum::serve(listener, app).await.unwrap();
        });
        let result = async {
            with_agent(&node);
            let secret = "fixture-webhook-secret-0123456789";
            let settings = node
                .configure_triage(json!({"default_outlet":"record","projects":[{"prefix":"/hooked","outlet":"both"}],
                    "webhook_url":url,"scan_codex_sessions":false,"webhook_secret":secret}))
                .await?;
            assert_eq!(settings["webhook_secret_set"], true);
            assert!(!settings.to_string().contains(secret));
            assert!(!std::fs::read_to_string(dir.join("config.json"))?.contains(secret));

            let first = node.ingest_agent_event(turn("s1", "t1", "/hooked/app")).await?;
            assert_eq!(first["status"], "accepted");
            let scanned = AgentEvent { source: triage::Source::SessionScan, status: Some("completed".into()), ..turn("s1", "t1", "/hooked/app") };
            assert_eq!(node.ingest_agent_event(scanned).await?["status"], "duplicate");
            let test = node.ingest_agent_event(AgentEvent { test: true, ..turn("s9", "t9", "/x") }).await?;
            assert_eq!(test["test"], true);
            node.ingest_agent_event(turn("s1", "t2", "/hooked/app")).await?;
            node.ingest_agent_event(turn("s2", "t1", "/elsewhere")).await?;
            let pending = node.triage_command(json!({"action":"events"})).await?;
            assert_eq!(pending["events"].as_array().unwrap().len(), 3);

            let event_id = first["event_id"].as_str().unwrap();
            let raise = json!({"action":"raise","event_id":event_id,"severity":"urgent","summary":"测试失败","matter":"codex:s1"});
            let raised = node.triage_command(raise.clone()).await?;
            assert_eq!((raised["sent"].clone(), raised["recorded"].clone(), raised["queued_webhook"].clone()), (json!(true), json!(true), json!(true)));
            assert_eq!(node.triage_command(raise).await?, raised, "a repeated raise answers the same");
            let pending = node.triage_command(json!({"action":"events"})).await?;
            let rest: Vec<String> = pending["events"].as_array().unwrap().iter().map(|e| e["event_id"].as_str().unwrap().to_owned()).collect();
            let same_matter = node.triage_command(json!({"action":"raise","event_id":rest[0],"severity":"attention","summary":"again","matter":"codex:s1"})).await?;
            assert_eq!((same_matter["sent"].clone(), same_matter["count"].clone()), (json!(false), json!(2)));
            let quiet = node.triage_command(json!({"action":"dismiss","event_id":rest[1]})).await?;
            assert_eq!(quiet["dismissed"], true);
            assert!(node.triage_command(json!({"action":"raise","event_id":"unknown","severity":"info","summary":"x","matter":"m"})).await.is_err());
            assert_eq!(node.triage_command(json!({"action":"events"})).await?["events"], json!([]));

            let network = node.network_id().await?;
            let alerts = node.store.lock().await.alerts(&network, 10)?;
            assert_eq!(alerts.len(), 1);
            assert_eq!((alerts[0]["severity"].clone(), alerts[0]["summary"].clone(), alerts[0]["resolution"].clone()), (json!("urgent"), json!("测试失败"), Value::Null));

            node.deliver_webhooks().await?;
            node.deliver_webhooks().await?;
            let received = received.lock().unwrap().clone();
            assert_eq!(received.len(), 1, "delivered once");
            let (headers, body) = &received[0];
            let timestamp: i64 = headers["x-vibeface-timestamp"].to_str()?.parse()?;
            assert_eq!(headers["x-vibeface-event-id"], event_id);
            assert_eq!(headers["x-vibeface-signature"].to_str()?, triage::signature(secret.as_bytes(), timestamp, body));
            let body: Value = serde_json::from_str(body)?;
            assert_eq!(body["decision"]["thread_id"], "s1");
            assert_eq!(node.triage_settings().await?["outbox"]["delivered"], 1);
            assert_eq!(node.test_webhook().await?["ok"], true);

            let alert_id = alerts[0]["alert_id"].as_str().unwrap();
            let body = admin.attest(&network, ATTENTION_SCOPE, EventBody::AttentionResolved {
                alert_id: alert_id.into(), resolution: AlertResolution::Dismissed, attestation: None })?;
            let access = Access { network_id: network.clone(), human: true, conversation_id: None, manager: None };
            node.resolve_alert(&access, &json!({"alert_id":alert_id,"resolution":"dismissed","attestation":body.attestation()})).await?;
            assert_eq!(node.alerts(&access).await?["alerts"][0]["resolution"], "dismissed");
            Ok::<_, anyhow::Error>(())
        }
        .await;
        server.abort();
        drop(node);
        std::fs::remove_dir_all(dir)?;
        result
    }

    #[tokio::test]
    async fn decision_tokens_are_shown_once_stored_hashed_and_revocable() -> Result<()> {
        let (node, _, dir) = node().await?;
        let result = async {
            let created = node.create_decision_token("grok").await?;
            let token = created["token"].as_str().unwrap().to_owned();
            let stored = std::fs::read_to_string(dir.join(DECISION_TOKENS))?;
            assert!(!stored.contains(&token) && stored.contains(&crate::crypto::digest(token.as_bytes())));
            assert!(!node.triage_settings().await?.to_string().contains(&token));
            assert!(node.decision_token_valid(&token) && !node.decision_token_valid("vfd_wrong"));
            let decision = json!({"device_id":node.identity.device_id()?,"thread_id":"t","text":"go","decision_id":"d1"});
            let refused = node.decision_http("vfd_wrong", decision.clone()).await.unwrap_err();
            assert!(format!("{refused:#}").contains("invalid decision token"));
            node.revoke_decision_token(created["token_id"].as_str().unwrap()).await?;
            assert!(!node.decision_token_valid(&token));
            Ok::<_, anyhow::Error>(())
        }
        .await;
        let _ = std::fs::remove_file(&node.codex.socket);
        drop(node);
        std::fs::remove_dir_all(dir)?;
        result
    }
}
