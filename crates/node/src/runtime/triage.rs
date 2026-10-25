//! Triage workers (design 4): capture into one queue, hand batches to the coordination Agent,
//! ask the user in the chat of the bot that owns each session, deliver webhooks from a
//! local outbox, hold App Server requests for the Agent, and give sessions no bot started
//! to the primary bot until it hands them to another or stops following them.
use super::{Access, Changed, Runtime, now_ms};
use crate::{
    config,
    manager::{self, ManagerConfig},
    model::{Choice, EventBody, Subject},
    net,
    triage::{self, AgentEvent, Delivery, OutboxEntry, Severity, TriageConfig},
};
use anyhow::{Context as _, Result, bail, ensure};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::{Value, json};
use std::{
    collections::{HashMap, HashSet},
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
/// The session of an App Server request that belongs to no thread.
const NO_THREAD: &str = "app-server";
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
    /// App Server threads nobody drives (`codex exec`, sub-agents); no bot is given them.
    background: std::sync::Mutex<HashSet<String>>,
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
            background: Default::default(),
            started_ms: now_ms(),
        }
    }
    pub(super) fn config(&self) -> TriageConfig {
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
    /// `None` while a question holds it for the user.
    answer_by_ms: Option<i64>,
    question_id: Option<String>,
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

#[derive(Clone, Serialize, Deserialize)]
struct DecisionToken {
    token_id: String,
    hash: String,
    label: String,
    created_ms: i64,
}

/// An answer from the webhook's side: one of the question's choices, or text.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DecisionInput {
    device_id: String,
    question_id: String,
    #[serde(default)]
    choice: Option<usize>,
    #[serde(default)]
    text: Option<String>,
}

/// One line of what a session last said.
fn summary(event: &AgentEvent) -> String {
    [&event.last_message, &event.detail]
        .into_iter()
        .flat_map(|text| text.lines())
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or_default()
        .chars()
        .take(120)
        .collect()
}

fn subject(event: &AgentEvent, request_id: Option<String>) -> Result<Subject> {
    Ok(Subject {
        node: event.node.clone(),
        agent: name(event.agent)?,
        session_id: event.session_id.clone(),
        cwd: event.cwd.clone(),
        summary: summary(event),
        request_id,
    })
}

fn shared_event(event: &AgentEvent, roots: &[PathBuf]) -> AgentEvent {
    crate::logging::add_private_path(Path::new(&event.cwd));
    let mut shared = event.clone();
    shared.cwd = roots
        .iter()
        .enumerate()
        .filter_map(|(index, root)| {
            let relative = Path::new(&event.cwd).strip_prefix(root).ok()?;
            if relative
                .components()
                .any(|part| matches!(part, std::path::Component::ParentDir))
            {
                return None;
            }
            Some((
                root.components().count(),
                format!("workspace-{}/{}", index + 1, relative.display()),
            ))
        })
        .max_by_key(|(depth, _)| *depth)
        .map_or_else(|| "workspace".into(), |(_, path)| path);
    shared.host.clear();
    shared.repo = None;
    shared.git_branch = None;
    shared.git_head = None;
    shared.last_message = crate::logging::scrub(&event.last_message);
    shared.detail = crate::logging::scrub(&event.detail);
    shared
}

fn name(value: impl Serialize) -> Result<String> {
    Ok(serde_json::to_value(value)?
        .as_str()
        .context("not a unit variant")?
        .to_owned())
}

fn from_name<T: DeserializeOwned>(name: String) -> Result<T> {
    Ok(serde_json::from_value(Value::String(name))?)
}

fn queued(connection: &Connection) -> Result<Vec<AgentEvent>> {
    let mut statement =
        connection.prepare("SELECT event FROM triage_queue ORDER BY ts_ms,event_id")?;
    let rows = statement.query_map([], |r| r.get::<_, String>(0))?;
    rows.map(|row| Ok(serde_json::from_str(&row?)?)).collect()
}

/// The bot that owns this device's session. A thread this device's work bound counts at
/// once: the App Server can ask before the turn's start is recorded. A bot's own reply thread
/// belongs to its conversation.
fn owner(store: &crate::store::Store, network: &str, event: &AgentEvent) -> Result<Option<String>> {
    if let Some(owner) = store.session_owner(network, &event.node, &event.session_id)? {
        return Ok(Some(owner));
    }
    if let Some(conversation) = store.native_thread_conversation(&event.session_id)? {
        return Ok(Some(conversation));
    }
    Ok(store
        .connection
        .query_row(
            "SELECT conversation_id FROM work_runs WHERE thread_id=? ORDER BY work_id LIMIT 1",
            [&event.session_id],
            |r| r.get(0),
        )
        .optional()?)
}

/// A turn this device's coordination Agent or a bot's reply runs: it answers in its own chat,
/// so its events are not news for the Agent. A request a bot's reply thread holds still goes
/// to the Agent, the only one who can answer it.
fn own_turn(store: &crate::store::Store, event: &AgentEvent) -> Result<bool> {
    let coordination = store
        .connection
        .query_row(
            "SELECT 1 FROM coordination_threads WHERE thread_id=?",
            [&event.session_id],
            |_| Ok(()),
        )
        .optional()?
        .is_some();
    Ok(coordination
        || (event.request_id.is_none()
            && store
                .native_thread_conversation(&event.session_id)?
                .is_some()))
}

fn dequeue(connection: &Connection, event: &AgentEvent) -> Result<()> {
    connection.execute(
        "DELETE FROM triage_queue WHERE ts_ms=? AND event_id=?",
        params![event.ts_ms.max(0), event.event_id],
    )?;
    Ok(())
}

fn outbox_from(connection: &Connection) -> Result<Vec<OutboxEntry>> {
    let mut statement = connection.prepare(
        "SELECT event_id,body,state,attempts,next_ms FROM triage_outbox ORDER BY event_id",
    )?;
    let rows = statement.query_map([], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, String>(2)?,
            r.get(3)?,
            r.get(4)?,
        ))
    })?;
    rows.map(|row| {
        let (event_id, body, state, attempts, next_ms) = row?;
        Ok(OutboxEntry {
            event_id,
            body,
            state: from_name(state)?,
            attempts,
            next_ms,
        })
    })
    .collect()
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
    /// dropped; so is everything while no coordination Agent is configured to judge it, and
    /// everything of a session its bot stopped following. A session no bot owns, or whose bot
    /// was deleted, goes to the primary bot first; without a bot its events drop. An event ID
    /// seen before is a duplicate, whichever source sent it first.
    pub(crate) async fn ingest_agent_event(&self, event: AgentEvent) -> Result<Value> {
        self.queue_event(AgentEvent {
            request_id: None,
            ..event
        })
        .await
    }

    async fn queue_event(&self, event: AgentEvent) -> Result<Value> {
        crate::logging::add_private_path(Path::new(&event.cwd));
        let event = event.seal(&self.identity.device_id()?, config::UNNAMED_NODE, now_ms())?;
        if event.test {
            tracing::info!(event_id = event.event_id, "triage test event received");
            return Ok(json!({"status":"accepted","event_id":event.event_id,"test":true}));
        }
        let network = self.network_id().await?;
        if self.manager().is_none() {
            return Ok(json!({"status":"dropped","event_id":event.event_id}));
        }
        let mut store = self.store.lock().await;
        if own_turn(&store, &event)? {
            return Ok(json!({"status":"own","event_id":event.event_id}));
        }
        if store.session_muted(&network, &event.node, &event.session_id)? {
            return Ok(json!({"status":"muted","event_id":event.event_id}));
        }
        if owner(&store, &network, &event)?.is_none()
            && self.claim_session(&mut store, &network, &event)?.is_none()
        {
            return Ok(json!({"status":"foreign","event_id":event.event_id}));
        }
        let first = store.connection.execute(
            "INSERT INTO triage_seen(event_id,ts_ms) VALUES(?,?) ON CONFLICT(event_id) DO NOTHING",
            params![event.event_id, now_ms()],
        )?;
        if first == 0 {
            return Ok(json!({"status":"duplicate","event_id":event.event_id}));
        }
        store.connection.execute(
            "INSERT INTO triage_queue(ts_ms,event_id,request_id,event) VALUES(?,?,?,?)
             ON CONFLICT(ts_ms,event_id) DO UPDATE SET request_id=excluded.request_id,event=excluded.event",
            params![
                event.ts_ms.max(0),
                event.event_id,
                event.request_id,
                serde_json::to_string(&event)?
            ],
        )?;
        Ok(json!({"status":"accepted","event_id":event.event_id}))
    }

    /// Gives a session this device runs and no bot owns to the primary bot, written once.
    /// Returns that bot's conversation, or `None` when there is no bot yet or nobody drives
    /// the session.
    fn claim_session(
        &self,
        store: &mut crate::store::Store,
        network: &str,
        event: &AgentEvent,
    ) -> Result<Option<String>> {
        if event.session_id == NO_THREAD
            || lock(&self.triage.background).contains(&event.session_id)
        {
            return Ok(None);
        }
        let Some(primary) = store.primary_bot(network)? else {
            return Ok(None);
        };
        let written = store.claim_session(
            &self.identity,
            network,
            &primary,
            &subject(&shared_event(event, &self.settings_snapshot().roots), None)?,
            now_ms(),
        )?;
        if let Some(written) = written {
            tracing::info!(
                event = "session.attributed",
                conversation_id = primary,
                thread_id = event.session_id,
                "a session no bot started went to the primary bot"
            );
            self.publish_changed(network, &primary, Changed::Event(&written.hash));
        }
        Ok(Some(primary))
    }

    /// A server request the bridge handed over: held for the coordination Agent, or answered by
    /// the fallback at once when there is none or no bot owns its thread.
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
            question_id: None,
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
            session_id: request.thread_id.clone().unwrap_or(NO_THREAD.into()),
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
        self.store
            .lock()
            .await
            .connection
            .execute("DELETE FROM triage_queue WHERE request_id=?", [request_id])?;
        Ok(outcome)
    }

    /// Requests past their deadline get the fallback answer. One a question holds gets its
    /// deadline once the question is answered, so the bot can act on the answer first.
    async fn expire_codex_requests(&self) -> Result<()> {
        let now = now_ms();
        let requests: Vec<CodexRequest> = lock(&self.triage.requests).values().cloned().collect();
        for request in requests {
            if let (Some(question), None) = (&request.question_id, request.answer_by_ms) {
                let network = self.network_id().await?;
                let answered = self
                    .store
                    .lock()
                    .await
                    .question(&network, question)?
                    .is_some_and(|question| !question["answer"].is_null());
                if answered
                    && let Some(held) = lock(&self.triage.requests).get_mut(&request.request_id)
                {
                    held.answer_by_ms = Some(now + AGENT_ANSWER_WITHIN.as_millis() as i64);
                }
            } else if request.answer_by_ms.is_some_and(|by| by <= now) {
                self.answer_codex_request(&request.request_id, None, "fallback")
                    .await?;
            }
        }
        Ok(())
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
                if !triage::interactive_codex(&params["thread"]["source"], None) {
                    let mut background = lock(&self.triage.background);
                    // ponytail: forget past 1024 threads, as above.
                    if background.len() > 1024 {
                        background.clear();
                    }
                    background.insert(thread);
                }
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
                if let Err(error) = node.send_answered().await {
                    tracing::warn!(%error, "could not tell the webhook about answers");
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

    async fn pending_events(&self) -> Result<Vec<AgentEvent>> {
        queued(&self.store.lock().await.connection)
    }

    /// One turn of this device's coordination Agent; it acts through the `vibeface` CLI.
    async fn coordination_turn(&self, prompt: impl FnOnce(&str) -> Result<String>) -> Result<()> {
        let Some(settings) = self.manager() else {
            return Ok(());
        };
        if settings.provider == manager::ManagerKind::CodexSdk {
            self.codex.ready().await;
        }
        let program = std::env::current_exe()?;
        let cli = format!("'{}'", program.to_string_lossy().replace('\'', r"'\''"));
        let config = ManagerConfig {
            tool_cli: Some(program),
            ..self.manager_config(settings)
        };
        let (_cancel, cancel) = tokio::sync::watch::channel(false);
        // Kept before the turn starts, so its hooks, scans and App Server events are known.
        let record = |thread: String| async move {
            self.store.lock().await.connection.execute(
                "INSERT INTO coordination_threads(thread_id) VALUES(?)
                 ON CONFLICT(thread_id) DO NOTHING",
                [thread],
            )?;
            Ok(())
        };
        manager::run_manager(&config, &prompt(&cli)?, None, record, cancel, |_| async {
            Ok(())
        })
        .await
        .map(|_| ())
    }

    /// Hands waiting events, each with the bot that owns its session, to one turn of the
    /// coordination Agent. It answers through `vibeface triage` and `vibeface codex respond`;
    /// what it leaves alone is dropped after the turn, and a request it neither answered nor
    /// raised gets the fallback answer.
    async fn wake_agent(&self) -> Result<()> {
        if self.manager().is_none() {
            return Ok(());
        }
        let fresh: Vec<AgentEvent> = self
            .pending_events()
            .await?
            .into_iter()
            .take(BATCH)
            .collect();
        if fresh.is_empty() {
            return Ok(());
        }
        let network = self.network_id().await?;
        let mut offered = Vec::new();
        {
            let store = self.store.lock().await;
            for event in &fresh {
                let owner = owner(&store, &network, event)?;
                let name = match &owner {
                    Some(scope) => match store.bot_for_conversation(&network, scope)? {
                        Some(bot) => Some(bot.name),
                        None => store.conversation(&network, scope)?.map(|c| c.title),
                    },
                    None => None,
                };
                offered.push((
                    shared_event(event, &self.settings_snapshot().roots),
                    json!({"conversation_id":owner,"name":name}),
                ));
            }
        }
        let turn = self
            .coordination_turn(|cli| {
                triage::wake_prompt(&offered, cli, AGENT_ANSWER_WITHIN.as_secs())
            })
            .await;
        self.settle_offered(&fresh).await?;
        turn
    }

    async fn settle_offered(&self, offered: &[AgentEvent]) -> Result<()> {
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
            dequeue(&self.store.lock().await.connection, event)?;
        }
        Ok(())
    }

    /// `/api/triage`, the Agents' side: `requests`, `respond`, `events`, `raise` and `dismiss`.
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
            if let Some(node) = input["node"].as_str()
                && node != self.identity.device_id()?
            {
                return self
                    .peer_request(
                        &network,
                        node,
                        json!({"type":"codex_respond","request_id":request_id,"result":result}),
                    )
                    .await;
            }
            return self
                .answer_codex_request(request_id, Some(Ok(result)), "agent")
                .await;
        }
        if action == "events" {
            let since = input["since"].as_i64().unwrap_or(0);
            let events: Vec<_> = self
                .pending_events()
                .await?
                .into_iter()
                .filter(|event| event.ts_ms >= since)
                .collect();
            return Ok(json!({"events":events}));
        }
        let event_id = super::id(&input, "event_id")?;
        {
            let store = self.store.lock().await;
            let handled: Option<String> = store
                .connection
                .query_row(
                    "SELECT result FROM triage_handled WHERE event_id=?",
                    [&event_id],
                    |r| r.get(0),
                )
                .optional()?;
            if let Some(handled) = handled {
                return Ok(serde_json::from_str(&handled)?);
            }
        }
        let event = self
            .pending_events()
            .await?
            .into_iter()
            .find(|event| event.event_id == event_id)
            .context("no pending event with this ID")?;
        let result = match action {
            "dismiss" => json!({"event_id":event_id,"dismissed":true}),
            "raise" => {
                let severity: Severity = serde_json::from_value(input["severity"].clone())
                    .context("severity must be info, attention or urgent")?;
                let labels: Vec<String> =
                    serde_json::from_value(input.get("choices").cloned().unwrap_or(json!([])))
                        .context("choices must be labels")?;
                let request_id = input["request_id"].as_str().map(str::to_owned);
                if let Some(request_id) = &request_id {
                    ensure!(
                        lock(&self.triage.requests).contains_key(request_id),
                        "no App Server request is waiting with this ID"
                    );
                }
                let question = EventBody::QuestionAsked {
                    question_id: triage::question_id(&event.node, &event.event_id),
                    severity,
                    text: crate::logging::scrub(super::field(&input, "question")?),
                    choices: labels
                        .into_iter()
                        .map(|label| Choice {
                            label: crate::logging::scrub(&label),
                        })
                        .collect(),
                    about: subject(
                        &shared_event(&event, &self.settings_snapshot().roots),
                        request_id,
                    )?,
                };
                self.raise(&network, &event, question).await?
            }
            _ => bail!("unknown triage action"),
        };
        let store = self.store.lock().await;
        store.connection.execute(
            "INSERT INTO triage_handled(event_id,ts_ms,result) VALUES(?,?,?)
             ON CONFLICT(event_id) DO UPDATE SET ts_ms=excluded.ts_ms,result=excluded.result",
            params![event_id, now_ms(), result.to_string()],
        )?;
        dequeue(&store.connection, &event)?;
        Ok(result)
    }

    /// Asks in the chat of the bot that owns the event's session, the primary bot's when its
    /// bot was deleted after the event came. An unanswered question about the same session and
    /// request stands instead of a new one; a request it names waits for the answer.
    async fn raise(&self, network: &str, event: &AgentEvent, question: EventBody) -> Result<Value> {
        let (asked, hash, owner) = {
            let mut store = self.store.lock().await;
            let owner = match owner(&store, network, event)? {
                Some(owner) => owner,
                None => self
                    .claim_session(&mut store, network, event)?
                    .context("no bot owns this session")?,
            };
            let (asked, written) =
                store.ask(&self.identity, network, &owner, question, now_ms())?;
            (asked, written.map(|event| event.hash), owner)
        };
        let question_id = asked["question_id"].as_str().unwrap_or_default().to_owned();
        if let Some(request_id) = asked["about"]["request_id"].as_str()
            && let Some(request) = lock(&self.triage.requests).get_mut(request_id)
            && request.question_id.is_none()
        {
            request.answer_by_ms = None;
            request.question_id = Some(question_id.clone());
        }
        let mut sent = false;
        if let Some(hash) = &hash {
            tracing::info!(
                event = "question.asked",
                conversation_id = owner,
                thread_id = event.session_id,
                question_id,
                "question asked in the bot chat"
            );
            self.publish_changed(network, &owner, Changed::Event(hash));
            sent = self.send_question(&asked, &event.cwd).await?;
        }
        Ok(
            json!({"event_id":event.event_id,"question_id":question_id,"conversation_id":owner,
            "asked":hash.is_some(),"queued_webhook":sent}),
        )
    }

    /// Queues a new question for the webhook when its project's outlet says so.
    async fn send_question(&self, question: &Value, cwd: &str) -> Result<bool> {
        let config = self.triage.config();
        if config.outlet_for(cwd) != triage::Outlet::Both || config.webhook_url.is_none() {
            return Ok(false);
        }
        let question_id = question["question_id"].as_str().unwrap_or_default();
        let choices: Vec<Value> = question["choices"]
            .as_array()
            .into_iter()
            .flatten()
            .enumerate()
            .map(|(index, choice)| json!({"index":index,"label":choice["label"]}))
            .collect();
        let body = json!({
            "schema_version":triage::SCHEMA_VERSION, "type":"question", "event_id":question_id,
            "question_id":question_id, "severity":question["severity"], "text":question["text"],
            "choices":choices, "about":question["about"], "bot":question["bot"],
            "answer":{"url":self.decision_url(), "device_id":self.identity.device_id()?,
                "question_id":question_id},
        });
        self.queue_webhook(question_id, &body).await?;
        Ok(true)
    }

    /// Tells the webhook once per answer that stands on a question it was sent, wherever it was
    /// given. When answers race, sync replaces this device's answer with the winner; the winner
    /// then goes out under its own event ID, and an unsent earlier one is dropped.
    async fn send_answered(&self) -> Result<()> {
        let Ok(network) = self.network_id().await else {
            return Ok(());
        };
        let answered: Vec<(String, String, String)> = {
            let store = self.store.lock().await;
            let mut statement = store.connection.prepare(
                "SELECT q.question_id,m.text,'answered-'||q.question_id||'-'||substr(m.event_hash,1,16) FROM triage_outbox o
                 JOIN questions q ON q.network_id=?1 AND q.question_id=o.event_id
                 JOIN messages m ON m.network_id=q.network_id AND m.conversation_id=q.conversation_id AND m.answers=q.question_id
                 WHERE NOT EXISTS(SELECT 1 FROM triage_outbox a WHERE a.event_id='answered-'||q.question_id||'-'||substr(m.event_hash,1,16))",
            )?;
            statement
                .query_map([&network], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
                .collect::<rusqlite::Result<_>>()?
        };
        for (question_id, text, event_id) in answered {
            self.store.lock().await.connection.execute(
                "DELETE FROM triage_outbox WHERE event_id LIKE 'answered-'||?||'-%' AND state=?",
                params![question_id, name(Delivery::Pending)?],
            )?;
            let body = json!({"schema_version":triage::SCHEMA_VERSION,"type":"answered",
                "event_id":event_id,"question_id":question_id,"text":text});
            self.queue_webhook(&event_id, &body).await?;
        }
        Ok(())
    }

    /// One outbox entry per event ID, however often it is queued.
    async fn queue_webhook(&self, event_id: &str, body: &Value) -> Result<()> {
        let entry = OutboxEntry::new(event_id, body.to_string(), now_ms());
        self.store.lock().await.connection.execute(
            "INSERT INTO triage_outbox(event_id,body,state,attempts,next_ms)
             VALUES(?,?,?,?,?) ON CONFLICT(event_id) DO NOTHING",
            params![
                entry.event_id,
                entry.body,
                name(entry.state)?,
                entry.attempts,
                entry.next_ms,
            ],
        )?;
        self.triage.outbox.notify_one();
        Ok(())
    }

    /// `from`'s bot hands a session it follows to `target`, or stops following it when `None`,
    /// at the version it read. The device running the session writes the change, so another
    /// device's session goes there over peer RPC. Asking for what stands writes nothing.
    pub(super) async fn hand_over_session(
        &self,
        network: &str,
        from: &str,
        node: &str,
        session: &str,
        seen: &str,
        target: Option<&str>,
    ) -> Result<Value> {
        if node != self.identity.device_id()? {
            return self
                .peer_request(
                    network,
                    node,
                    json!({"type":"hand_over_session","session_id":session,"version":seen,"from":from,"conversation_id":target}),
                )
                .await;
        }
        let (before, version, event) = self.store.lock().await.hand_over_session(
            &self.identity,
            network,
            from,
            session,
            seen,
            target,
            now_ms(),
        )?;
        let owner = target.unwrap_or(&before);
        if let Some(event) = &event {
            tracing::info!(
                event = if target.is_some() {
                    "session.attributed"
                } else {
                    "session.muted"
                },
                conversation_id = owner,
                thread_id = session,
                "a bot changed who follows a session"
            );
            self.publish_changed(network, owner, Changed::Event(&event.hash));
            if owner != before {
                self.publish_changed(network, &before, Changed::Reload);
            }
        }
        Ok(
            json!({"node":node,"session_id":session,"conversation_id":owner,"muted":target.is_none(),
            "version":version,"changed":event.is_some()}),
        )
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
        let entries = outbox_from(&self.store.lock().await.connection)?;
        for mut entry in entries {
            if entry.state != Delivery::Pending || entry.next_ms > now_ms() {
                continue;
            }
            let (status, error) = post_webhook(&url, &secret, &entry.event_id, &entry.body).await;
            entry.record(status, now_ms());
            if entry.state == Delivery::Dead {
                tracing::warn!(
                    event_id = entry.event_id,
                    error = error.or_else(|| status.map(|value| format!("HTTP {value}"))),
                    "webhook delivery dead-lettered"
                );
            }
            self.store.lock().await.connection.execute(
                "UPDATE triage_outbox SET state=?,attempts=?,next_ms=? WHERE event_id=?",
                params![
                    name(entry.state)?,
                    entry.attempts,
                    entry.next_ms,
                    entry.event_id
                ],
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
        if !self.has_membership().await? {
            return Ok(());
        }
        // The coordination Agent's threads stay known: one forgotten would go to the primary bot.
        {
            let store = self.store.lock().await;
            let cutoff = now_ms() - SEEN_TTL_MS;
            store
                .connection
                .execute("DELETE FROM triage_seen WHERE ts_ms<?", [cutoff])?;
            store
                .connection
                .execute("DELETE FROM triage_handled WHERE ts_ms<?", [cutoff])?;
        }
        let config = self.triage.config();
        if !config.scan_codex_sessions {
            return Ok(());
        }
        let root = config
            .codex_sessions
            .as_ref()
            .context("codex_sessions is required")?;
        for path in rollouts(root, now_ms() - SCAN_WINDOW_MS) {
            let rollout = crate::crypto::digest(path.to_string_lossy().as_bytes());
            let mark: Option<usize> = self
                .store
                .lock()
                .await
                .connection
                .query_row(
                    "SELECT scanned_to FROM triage_scans WHERE rollout=?",
                    [&rollout],
                    |r| r.get(0),
                )
                .optional()?;
            let Ok(text) = std::fs::read_to_string(&path) else {
                continue;
            };
            let offset = match mark {
                Some(mark) => mark,
                None if modified_ms(&path) < self.triage.started_ms => text.len(),
                None => 0,
            };
            let (events, end) = triage::scan_rollout(&text, offset);
            for event in events {
                if let Err(error) = self.ingest_agent_event(event).await {
                    tracing::debug!(%error, "scanned turn was not triaged");
                }
            }
            self.store.lock().await.connection.execute(
                "INSERT INTO triage_scans(rollout,scanned_to) VALUES(?,?)
                 ON CONFLICT(rollout) DO UPDATE SET scanned_to=excluded.scanned_to",
                params![rollout, end],
            )?;
        }
        Ok(())
    }

    pub(crate) async fn triage_settings(&self) -> Result<Value> {
        let config = self.triage.config();
        let tokens: Vec<Value> = self
            .decision_tokens()?
            .into_iter()
            .map(|t| json!({"token_id":t.token_id,"label":t.label,"created_ms":t.created_ms}))
            .collect();
        let mut outbox = json!({"pending":0,"delivered":0,"dead":0});
        {
            let store = self.store.lock().await;
            let mut statement = store
                .connection
                .prepare("SELECT state,COUNT(*) FROM triage_outbox GROUP BY state")?;
            let rows =
                statement.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, u64>(1)?)))?;
            for row in rows {
                let (state, count) = row?;
                outbox[state] = json!(count);
            }
        }
        Ok(json!({
            "default_outlet":config.default_outlet, "projects":config.projects,
            "webhook_url":config.webhook_url, "scan_codex_sessions":config.scan_codex_sessions,
            "codex_sessions":config.codex_sessions,
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
            "summary":"Vibeface webhook test","device_id":self.identity.device_id()?})
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

    pub(super) async fn questions(&self, access: &Access) -> Result<Value> {
        Ok(json!({"questions":self.store.lock().await.unanswered_questions(&access.network_id)?}))
    }

    /// The user's answer, signed by their device, to a question in any chat.
    pub(super) async fn answer_question(&self, access: &Access, input: &Value) -> Result<Value> {
        ensure!(access.human, "only the user answers questions");
        let answered = self
            .post_answer(
                &access.network_id,
                &super::id(input, "question_id")?,
                super::field(input, "text")?,
                Some(super::attestation(input)?),
            )
            .await?;
        Ok(
            json!({"status":answered.0,"question_id":answered.1.answers,"message":answered.1,
            "receipt":answered.2}),
        )
    }

    /// Posts the one answer to a question; the conversation's owner turns it into an `answer`
    /// turn.
    async fn post_answer(
        &self,
        network: &str,
        question: &str,
        text: &str,
        attestation: Option<crate::model::Attestation>,
    ) -> Result<(
        &'static str,
        crate::model::Message,
        Option<crate::scheduling::AdmissionReceipt>,
    )> {
        let _work = self.maintenance.enter()?;
        ensure!(
            !text.trim().is_empty() && text.len() <= 4000,
            "an answer is 1-4000 bytes"
        );
        let answered = self.store.lock().await.accept_answer(
            &self.identity,
            network,
            question,
            text,
            attestation,
            now_ms(),
        )?;
        if answered.already_answered {
            return Ok(("already_answered", answered.message, None));
        }
        tracing::info!(
            event = "question.answered",
            conversation_id = answered.conversation_id,
            question_id = question,
            "question answered"
        );
        self.publish_changed(
            network,
            &answered.conversation_id,
            Changed::Event(&answered.message.event_hash),
        );
        if answered.receipt.is_some() {
            self.wake_scheduler()?;
        }
        Ok(("answered", answered.message, answered.receipt))
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

    /// A webhook's answer: written by this Node without the user's signature, as the same one
    /// answer the web gives. A retry reads the standing answer back.
    async fn local_decision(self: &Arc<Self>, token: &str, input: DecisionInput) -> Result<Value> {
        ensure!(self.decision_token_valid(token), "invalid decision token");
        ensure!(
            input.device_id == self.identity.device_id()?,
            "this decision is for another device"
        );
        let network = self.network_id().await?;
        let question = self
            .store
            .lock()
            .await
            .question(&network, &input.question_id)?
            .context("no such question")?;
        let text = match (input.choice, input.text) {
            (Some(index), None) => question["choices"][index]["label"]
                .as_str()
                .context("no such choice")?
                .to_owned(),
            (None, Some(text)) => text,
            _ => bail!("answer with either a choice or text"),
        };
        let (status, message, _) = self
            .post_answer(&network, &input.question_id, &text, None)
            .await?;
        Ok(
            json!({"question_id":input.question_id,"answer_id":message.message_id,
            "status":status,"text":message.text}),
        )
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
    };

    async fn bare_node() -> Result<(Arc<Runtime>, PathBuf)> {
        let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../.artifacts")
            .join(format!("triage-{}", uuid::Uuid::new_v4()));
        config::private_dir(&dir)?;
        let mut configuration = Config::local("127.0.0.1:0".parse()?, vec![], false);
        // A unix socket path must stay short; nothing listens unless a test binds it.
        let socket = format!(
            "/tmp/vf-{}.sock",
            &uuid::Uuid::new_v4().simple().to_string()[..12]
        );
        configuration.providers.codex_app_server.socket = Some(socket.into());
        configuration.providers.execution = true;
        configuration.save(&dir)?;
        config::private_write(
            &dir.join("data-epoch.json"),
            &crate::config::data_epoch_marker(),
        )?;
        config::private_write(
            &dir.join("identity.json"),
            &serde_json::to_vec(&LocalIdentity::generate())?,
        )?;
        Ok((Arc::new(Runtime::open(&dir).await?), dir))
    }

    async fn node() -> Result<(Arc<Runtime>, LocalIdentity, PathBuf)> {
        let (node, dir) = bare_node().await?;
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
            provider: crate::manager::ManagerKind::CodexSdk,
            workspace: "/tmp".into(),
            model: "fixture".into(),
            cursor_sdk_bin: "/nonexistent".into(),
        });
    }

    async fn bot(
        node: &Runtime,
        admin: &LocalIdentity,
        scope: &str,
        name: &str,
        threads: &[&str],
    ) -> Result<()> {
        let network = node.network_id().await?;
        let local = node.identity.device_id()?;
        let owner = admin.attest(
            &network,
            scope,
            EventBody::ConversationOwner {
                scheduler_node: local.clone(),
                epoch: 1,
                attestation: None,
                cause: None,
            },
        )?;
        let owner = crate::model::FirstOwner {
            scheduler_node: local,
            attestation: owner.attestation().cloned().context("signed owner")?,
        };
        let bot = admin.attest(
            &network,
            scope,
            EventBody::BotCreated {
                bot_id: scope.into(),
                name: name.into(),
                instructions: String::new(),
                project: None,
                attestation: None,
                cause: None,
            },
        )?;
        let mut store = node.store.lock().await;
        store.create_conversation(&node.identity, &network, scope, name, owner, now_ms())?;
        store.append_event(&node.identity, &network, scope, bot, vec![], now_ms())?;
        for thread in threads {
            let progress = EventBody::WorkProgress {
                work_id: format!("w-{thread}"),
                phase: crate::model::WorkPhase::Running,
                summary: String::new(),
                thread_id: Some((*thread).into()),
                turn_id: None,
            };
            store.append_event(&node.identity, &network, scope, progress, vec![], now_ms())?;
        }
        Ok(())
    }

    async fn unanswered(node: &Runtime) -> Result<Vec<Value>> {
        let network = node.network_id().await?;
        node.store.lock().await.unanswered_questions(&network)
    }

    async fn answer(node: &Runtime, question: &str, text: &str) -> Result<&'static str> {
        let network = node.network_id().await?;
        Ok(node.post_answer(&network, question, text, None).await?.0)
    }

    type Server = tokio_tungstenite::WebSocketStream<tokio::net::UnixStream>;

    fn named_socket(node: &Runtime) -> PathBuf {
        node.providers()
            .switches
            .codex_app_server
            .socket
            .expect("a named socket")
    }

    async fn read(server: &mut Server) -> Value {
        use futures_util::StreamExt as _;
        let message = server.next().await.unwrap().unwrap();
        serde_json::from_str(message.to_text().unwrap()).unwrap()
    }

    async fn app_server(node: &Runtime) -> Result<(Server, Value)> {
        use futures_util::SinkExt as _;
        let listener = tokio::net::UnixListener::bind(named_socket(node))?;
        let (stream, _) = tokio::time::timeout(Duration::from_secs(5), listener.accept()).await??;
        let mut server = tokio_tungstenite::accept_async(stream).await?;
        let initialize = read(&mut server).await;
        let ready = json!({"id":initialize["id"],"result":{"userAgent":"codex/fixture"}});
        server.send(ready.to_string().into()).await?;
        assert_eq!(read(&mut server).await["method"], "initialized");
        let params = json!({"threadId":"t-1","command":"rm -rf build","reason":"clean the build"});
        let request =
            json!({"id":41,"method":"item/commandExecution/requestApproval","params":params});
        server.send(request.to_string().into()).await?;
        while node.codex.client().snapshot().await["pending_requests"] == json!([]) {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let handed = json!({"id":41,"method":"item/commandExecution/requestApproval","threadId":"t-1","params":params});
        Ok((server, handed))
    }

    async fn answered_once(server: &mut Server) -> Value {
        let answer = read(server).await;
        assert_eq!(answer["id"], 41);
        let again = tokio::time::timeout(Duration::from_millis(300), read(server)).await;
        assert!(again.is_err(), "the request is answered once");
        answer
    }

    async fn cleanup(node: Arc<Runtime>, dir: PathBuf) -> Result<()> {
        let _ = std::fs::remove_file(named_socket(&node));
        drop(node);
        std::fs::remove_dir_all(dir)?;
        Ok(())
    }

    fn answering_app_server(node: &Runtime) -> Result<tokio::sync::mpsc::UnboundedReceiver<Value>> {
        use futures_util::{SinkExt as _, StreamExt as _};
        let listener = tokio::net::UnixListener::bind(named_socket(node))?;
        let (sent, received) = tokio::sync::mpsc::unbounded_channel();
        tokio::spawn(async move {
            let Ok((stream, _)) = listener.accept().await else {
                return;
            };
            let Ok(mut server) = tokio_tungstenite::accept_async(stream).await else {
                return;
            };
            while let Some(Ok(message)) = server.next().await {
                let Ok(request) = serde_json::from_slice::<Value>(&message.into_data()) else {
                    continue;
                };
                if request.get("id").is_some() {
                    let result = match request["method"].as_str() {
                        Some("initialize") => json!({"userAgent":"codex/fixture"}),
                        _ => json!({}),
                    };
                    let reply = json!({"id":request["id"],"result":result});
                    let _ = server.send(reply.to_string().into()).await;
                }
                let _ = sent.send(request);
            }
        });
        Ok(received)
    }

    #[tokio::test]
    async fn a_node_without_a_network_skips_the_session_scan() -> Result<()> {
        let (node, dir) = bare_node().await?;
        let result = node.scan_sources().await;
        std::fs::remove_dir_all(dir)?;
        result
    }

    #[tokio::test]
    async fn a_work_stopped_while_starting_is_interrupted_once_bound() -> Result<()> {
        let (node, admin, dir) = node().await?;
        let result = async {
            with_agent(&node);
            bot(&node, &admin, "c", "前端", &[]).await?;
            let mut requests = answering_app_server(&node)?;
            while node.codex.status().await["connection"]["state"] != "connected" {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            let network = node.network_id().await?;
            let local = node.identity.device_id()?;
            let spec = crate::work::WorkSpec {
                work_id: "w-stop".into(),
                conversation: "c".into(),
                trigger: "message:m1".into(),
                target: local.clone(),
                dispatcher: local,
                call_ref: "r".into(),
                kind: crate::model::WorkKind::Codex,
                cwd: "/tmp".into(),
                title: "t".into(),
                prompt: "p".into(),
                continues: None,
                parent: None,
                thread: Some("t-9".into()),
            };
            node.store
                .lock()
                .await
                .accept_work(&node.identity, &network, spec, now_ms())?;
            let stopped = node.interrupt_work(&network, "w-stop").await?;
            assert_eq!(stopped["waiting_for_start"], true);
            let waiting = node.store.lock().await.stop_requests()?;
            assert!(waiting.iter().any(|request| request.work_id == "w-stop"));

            let epoch = node.codex_epoch.load(std::sync::atomic::Ordering::SeqCst);
            let turn = crate::work::Signal::TurnStarted {
                turn_id: "u-1".into(),
                epoch,
            };
            let bound = node.signal_work(&network, "w-stop", turn).await?;
            assert!(matches!(
                bound.state,
                crate::work::WorkState::Interrupting { .. }
            ));
            let interrupt = loop {
                let request = tokio::time::timeout(Duration::from_secs(5), requests.recv())
                    .await?
                    .context("the App Server closed")?;
                if request["method"] == "turn/interrupt" {
                    break request;
                }
            };
            assert_eq!(
                interrupt["params"],
                json!({"threadId":"t-9","turnId":"u-1"})
            );
            assert!(node.store.lock().await.stop_requests()?.is_empty());
            node.deliver_work_requests(&network).await?;
            while let Ok(request) = requests.try_recv() {
                assert_ne!(request["method"], "turn/interrupt", "the stop is sent once");
            }
            Ok::<_, anyhow::Error>(())
        }
        .await;
        cleanup(node, dir).await?;
        result
    }

    #[tokio::test]
    async fn a_held_request_is_answered_by_the_agent_once() -> Result<()> {
        let (node, admin, dir) = node().await?;
        let result = async {
            with_agent(&node);
            bot(&node, &admin, "c", "前端", &["t-1"]).await?;
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
        cleanup(node, dir).await?;
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
        cleanup(node, dir).await?;
        result
    }

    #[tokio::test]
    async fn a_question_holds_the_request_until_it_is_answered() -> Result<()> {
        let (node, admin, dir) = node().await?;
        let result = async {
            with_agent(&node);
            bot(&node, &admin, "c", "前端", &["t-1"]).await?;
            let (mut server, handed) = app_server(&node).await?;
            node.on_codex_request(&handed).await;
            let event = node.triage_command(json!({"action":"events"})).await?["events"][0].clone();
            let request_id = event["request_id"].as_str().unwrap().to_owned();
            let raised = node
                .triage_command(json!({"action":"raise","event_id":event["event_id"],
                "severity":"urgent","question":"要清理 build 吗？","request_id":request_id,
                "choices":["允许","拒绝"]}))
                .await?;
            assert_eq!(
                (raised["asked"].clone(), raised["conversation_id"].clone()),
                (json!(true), json!("c"))
            );
            let question = raised["question_id"].as_str().unwrap().to_owned();
            node.expire_codex_requests().await?;
            let held =
                node.triage_command(json!({"action":"requests"})).await?["requests"][0].clone();
            assert_eq!(
                (held["answer_by_ms"].clone(), held["question_id"].clone()),
                (Value::Null, json!(question))
            );
            let asked = unanswered(&node).await?;
            assert_eq!(asked.len(), 1);
            assert_eq!(
                (
                    asked[0]["about"]["request_id"].clone(),
                    asked[0]["choices"][1]["label"].clone()
                ),
                (json!(request_id), json!("拒绝"))
            );

            assert_eq!(answer(&node, &question, "拒绝").await?, "answered");
            let jobs = node.store.lock().await.scheduler_jobs("c")?;
            assert_eq!(
                jobs.iter().map(|job| job.kind).collect::<Vec<_>>(),
                vec![crate::scheduling::TriggerKind::Answer]
            );
            node.expire_codex_requests().await?;
            let held =
                node.triage_command(json!({"action":"requests"})).await?["requests"][0].clone();
            assert!(
                held["answer_by_ms"].as_i64().is_some(),
                "the answer starts the bot's deadline"
            );
            let respond =
                json!({"action":"respond","request_id":request_id,"result":{"decision":"decline"}});
            node.triage_command(respond.clone()).await?;
            node.triage_command(respond).await?;
            assert_eq!(
                answered_once(&mut server).await["result"],
                json!({"decision":"decline"})
            );
            Ok::<_, anyhow::Error>(())
        }
        .await;
        cleanup(node, dir).await?;
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

    #[test]
    fn shared_events_keep_relative_context_without_local_identity() {
        let event = AgentEvent {
            host: "private-host".into(),
            repo: Some("/private-event-fixture/project".into()),
            last_message: "failed in /private-event-fixture/project/src".into(),
            ..turn("session", "turn", "/private-event-fixture/project")
        };
        let shared = shared_event(&event, &[PathBuf::from("/private-event-fixture")]);
        assert_eq!(shared.cwd, "workspace-1/project");
        assert!(shared.host.is_empty() && shared.repo.is_none());
        assert!(!shared.last_message.contains("/private-event-fixture"));
        assert_eq!(event.cwd, "/private-event-fixture/project");
        assert_eq!(shared.event_id, event.event_id);
        assert_eq!(shared_event(&event, &[]).cwd, "workspace");
    }

    async fn attributions(node: &Runtime, scope: &str) -> Result<usize> {
        let network = node.network_id().await?;
        let context = node.store.lock().await.context(&network, scope, None)?;
        Ok(context
            .events
            .iter()
            .filter(|event| matches!(event.event.body, EventBody::SessionAttributed { .. }))
            .count())
    }

    async fn owner_of(node: &Runtime, session: &str) -> Result<Option<String>> {
        let network = node.network_id().await?;
        let local = node.identity.device_id()?;
        node.store
            .lock()
            .await
            .session_owner(&network, &local, session)
    }

    /// The version of a session as its bot's chat lists it.
    async fn version_of(node: &Runtime, session: &str) -> Result<String> {
        let network = node.network_id().await?;
        let store = node.store.lock().await;
        for scope in ["c", "d", "e"] {
            if let Some(row) = store
                .attributed_sessions(&network, scope)?
                .into_iter()
                .find(|row| row["session_id"] == session)
            {
                return Ok(row["version"].as_str().context("version")?.to_owned());
            }
        }
        Ok(String::new())
    }

    #[tokio::test]
    async fn an_unowned_session_goes_to_the_primary_bot_once() -> Result<()> {
        let (node, admin, dir) = node().await?;
        let result = async {
            with_agent(&node);
            bot(&node, &admin, "c", "前端", &[]).await?;
            bot(&node, &admin, "d", "后端", &[]).await?;
            let network = node.network_id().await?;
            let local = node.identity.device_id()?;
            for step in ["t1", "t2"] {
                let queued = node.ingest_agent_event(turn("s1", step, "/w")).await?;
                assert_eq!(queued["status"], "accepted");
            }
            let again = node.ingest_agent_event(turn("s1", "t1", "/w")).await?;
            assert_eq!(again["status"], "duplicate");
            assert!(unanswered(&node).await?.is_empty(), "nobody is asked whose it is");
            assert_eq!(owner_of(&node, "s1").await?.as_deref(), Some("c"));
            assert_eq!((attributions(&node, "c").await?, attributions(&node, "d").await?), (1, 0));
            let lines = node.store.lock().await.attributed_sessions(&network, "c")?;
            assert_eq!(
                (lines[0]["session_id"].clone(), lines[0]["summary"].clone(), lines[0]["muted"].clone()),
                (json!("s1"), json!("tests fail"), json!(false))
            );

            let set = admin.attest(
                &network,
                crate::model::BOTS_SCOPE,
                EventBody::PrimaryBotSet {
                    conversation_id: "d".into(),
                    attestation: None,
                },
            )?;
            node.store.lock().await.append_event(
                &node.identity,
                &network,
                crate::model::BOTS_SCOPE,
                set,
                vec![],
                now_ms(),
            )?;
            node.ingest_agent_event(turn("s1", "t3", "/w")).await?;
            node.ingest_agent_event(turn("s2", "t1", "/w")).await?;
            assert_eq!(owner_of(&node, "s1").await?.as_deref(), Some("c"), "an owner stands");
            assert_eq!(owner_of(&node, "s2").await?.as_deref(), Some("d"), "the next goes to the new primary");
            assert_eq!(attributions(&node, "c").await?, 1);

            let (trigger, attempt) = start_reply(&node, &admin, "m1").await?;
            let idle = Access {
                network_id: network.clone(),
                human: false,
                conversation_id: Some("c".into()),
                manager: None,
            };
            node.register_manager_attempt("token".into(), idle.clone(), &attempt, &trigger)
                .await?;
            let live = Access {
                manager: Some(crate::runtime::ManagerAccess {
                    attempt_id: attempt.clone(),
                    trigger,
                }),
                ..idle.clone()
            };
            let hand = |access: &Access, session: &str, how: Value| {
                let (node, local, access, session) = (&node, &local, access.clone(), session.to_owned());
                async move {
                    let seen = version_of(node, &session).await?;
                    let mut args = json!({"node":local,"session_id":session,"version":seen});
                    args.as_object_mut().unwrap().extend(how.as_object().unwrap().clone());
                    node.tool(access, "hand_over_session", args).await
                }
            };
            let turn_result = async {
                assert!(
                    hand(&idle, "s1", json!({"to":"后端"})).await.is_err(),
                    "only a live bot turn hands a session over"
                );
                let theirs = hand(&live, "s2", json!({"to":"前端"})).await.expect_err("s2 is d's");
                assert!(theirs.to_string().contains("后端 follows it"), "{theirs:#}");
                assert!(hand(&live, "s1", json!({"to":"后端","stop_following":true})).await.is_err());

                let mute = json!({"stop_following":true});
                let muted = hand(&live, "s1", mute.clone()).await?;
                assert_eq!((muted["muted"].clone(), muted["changed"].clone()), (json!(true), json!(true)));
                assert_eq!(hand(&live, "s1", mute).await?["changed"], false, "a retried mute writes nothing");
                let quiet = node.ingest_agent_event(turn("s1", "t4", "/w")).await?;
                assert_eq!(quiet["status"], "muted");
                assert!(
                    node.pending_events().await?.iter().all(|event| event.event_id != quiet["event_id"]),
                    "a muted session's events drop"
                );
                assert_eq!(attributions(&node, "c").await?, 1, "a muted session is not claimed again");

                let to_d = json!({"to":"后端"});
                let moved = hand(&live, "s1", to_d.clone()).await?;
                assert_eq!(
                    (moved["conversation_id"].clone(), moved["muted"].clone(), moved["changed"].clone()),
                    (json!("d"), json!(false), json!(true))
                );
                let retried = hand(&live, "s1", to_d).await?;
                assert_eq!(retried["changed"], false, "a retried hand-over reads back what stands");
                assert_eq!(owner_of(&node, "s1").await?.as_deref(), Some("d"));
                assert_eq!(attributions(&node, "d").await?, 2, "each hand-over is written once");
                assert!(node.store.lock().await.attributed_sessions(&network, "c")?.is_empty());
                assert!(
                    hand(&live, "s1", json!({"stop_following":true})).await.is_err(),
                    "a session handed over is no longer the sender's"
                );

                let accepted = node.ingest_agent_event(turn("s1", "t5", "/w")).await?;
                let raise = json!({"action":"raise","event_id":accepted["event_id"],"severity":"attention","question":"测试失败，要修吗？"});
                assert_eq!(node.triage_command(raise).await?["conversation_id"], "d");
                Ok::<_, anyhow::Error>(())
            }
            .await;
            node.finish_manager_attempt(&attempt).await;
            turn_result
        }
        .await;
        cleanup(node, dir).await?;
        result
    }

    async fn delete_bot(node: &Runtime, admin: &LocalIdentity, scope: &str) -> Result<()> {
        let network = node.network_id().await?;
        let deleted = admin.attest(
            &network,
            scope,
            EventBody::BotDeleted {
                attestation: None,
                cause: None,
            },
        )?;
        let signed = deleted.attestation().cloned().context("signed")?;
        node.store
            .lock()
            .await
            .delete_bot(&node.identity, &network, scope, signed, now_ms())?
            .context("deleted")?;
        Ok(())
    }

    #[tokio::test]
    async fn a_deleted_bots_sessions_go_to_the_primary_bot() -> Result<()> {
        let (node, admin, dir) = node().await?;
        let result = async {
            with_agent(&node);
            bot(&node, &admin, "c", "前端", &[]).await?;
            bot(&node, &admin, "d", "后端", &[]).await?;
            bot(&node, &admin, "e", "运维", &[]).await?;
            let network = node.network_id().await?;
            for session in ["s1", "s2", "s3"] {
                node.ingest_agent_event(turn(session, "t1", "/w")).await?;
            }
            for (from, session, to) in [
                ("c", "s1", Some("d")),
                ("c", "s2", Some("d")),
                ("d", "s2", None),
                ("c", "s3", Some("e")),
            ] {
                let seen = version_of(&node, session).await?;
                node.store.lock().await.hand_over_session(
                    &node.identity,
                    &network,
                    from,
                    session,
                    &seen,
                    to,
                    now_ms(),
                )?;
            }
            let queued = node.ingest_agent_event(turn("s1", "t2", "/w")).await?;
            assert_eq!(queued["status"], "accepted");
            delete_bot(&node, &admin, "d").await?;

            assert_eq!(owner_of(&node, "s3").await?.as_deref(), Some("e"), "another bot's session stays");
            assert_eq!(owner_of(&node, "s1").await?, None, "a deleted bot follows nothing");
            let raise = json!({"action":"raise","event_id":queued["event_id"],"severity":"attention","question":"测试失败，要修吗？"});
            assert_eq!(
                node.triage_command(raise).await?["conversation_id"], "c",
                "an event queued before the deletion asks in the primary bot's chat"
            );
            let spoke = node.ingest_agent_event(turn("s2", "t2", "/w")).await?;
            assert_eq!(spoke["status"], "accepted", "a deleted bot's mute no longer holds");
            for (session, step) in [("s1", "t3"), ("s3", "t2")] {
                node.ingest_agent_event(turn(session, step, "/w")).await?;
            }
            assert_eq!(owner_of(&node, "s1").await?.as_deref(), Some("c"));
            assert_eq!(owner_of(&node, "s2").await?.as_deref(), Some("c"));
            assert_eq!(owner_of(&node, "s3").await?.as_deref(), Some("e"));
            assert_eq!(attributions(&node, "c").await?, 5, "each session goes back to the primary bot once");

            delete_bot(&node, &admin, "c").await?;
            assert_eq!(node.store.lock().await.primary_bot(&network)?.as_deref(), Some("e"));
            node.ingest_agent_event(turn("s1", "t4", "/w")).await?;
            assert_eq!(
                owner_of(&node, "s1").await?.as_deref(),
                Some("e"),
                "with the primary bot deleted, its sessions go to the earliest bot left"
            );
            assert_eq!(attributions(&node, "e").await?, 2);
            Ok::<_, anyhow::Error>(())
        }
        .await;
        cleanup(node, dir).await?;
        result
    }

    #[tokio::test]
    async fn unattended_sessions_get_no_owner() -> Result<()> {
        let (node, admin, dir) = node().await?;
        let result = async {
            with_agent(&node);
            let (mut server, handed) = app_server(&node).await?;
            node.on_codex_request(&handed).await;
            assert_eq!(
                answered_once(&mut server).await["result"],
                json!({"decision":"decline"})
            );
            let status = node.ingest_agent_event(turn("s1", "t1", "/w")).await?;
            assert_eq!(status["status"], "foreign");
            assert_eq!(owner_of(&node, "s1").await?, None);

            bot(&node, &admin, "c", "前端", &[]).await?;
            node.triage_app_server(&json!({"method":"thread/started","params":{"thread":
                {"id":"sub","cwd":"/w","source":{"subAgent":"review"}}}}));
            let status = node.ingest_agent_event(turn("sub", "t1", "/w")).await?;
            assert_eq!(status["status"], "foreign");
            assert_eq!(owner_of(&node, "sub").await?, None);
            let status = node.ingest_agent_event(turn("s1", "t2", "/w")).await?;
            assert_eq!(status["status"], "accepted");
            assert_eq!(owner_of(&node, "s1").await?.as_deref(), Some("c"));
            assert_eq!(attributions(&node, "c").await?, 1);
            Ok::<_, anyhow::Error>(())
        }
        .await;
        cleanup(node, dir).await?;
        result
    }

    async fn start_reply(
        node: &Runtime,
        admin: &LocalIdentity,
        id: &str,
    ) -> Result<(String, String)> {
        let network = node.network_id().await?;
        let posted = admin.attest(
            &network,
            "c",
            EventBody::MessagePosted {
                message_id: id.into(),
                role: crate::model::Role::User,
                text: id.into(),
                attachments: vec![],
                report_results: Some(true),
                in_reply_to: None,
                status: None,
                answers: None,
                attestation: None,
            },
        )?;
        let signed = posted.attestation().cloned().context("signed")?;
        let mut store = node.store.lock().await;
        store.accept_message(
            &node.identity,
            &network,
            "c",
            id,
            id,
            vec![],
            true,
            signed,
            now_ms(),
        )?;
        let claimed = store
            .claim_next(&node.identity, &network, "c", "boot", "cfg")?
            .context("job")?;
        let trigger = claimed.job.trigger.clone();
        let attempt = claimed.job.state.attempt().context("attempt")?.id.clone();
        store.mark_started("c", &trigger, &attempt)?;
        Ok((trigger, attempt))
    }

    #[tokio::test]
    async fn a_check_from_another_app_server_stays_pending() -> Result<()> {
        let (node, admin, dir) = node().await?;
        let result = async {
            with_agent(&node);
            bot(&node, &admin, "c", "前端", &[]).await?;
            let _requests = answering_app_server(&node)?;
            while node.codex.status().await["connection"]["state"] != "connected" {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            let network = node.network_id().await?;
            assert_eq!(node.codex.server(), "system");
            for (id, server) in [("m1", "own:"), ("m2", "system")] {
                let (trigger, attempt) = start_reply(&node, &admin, id).await?;
                let mut store = node.store.lock().await;
                store.record_manager_turn(
                    &attempt,
                    server,
                    "thread",
                    Some("turn"),
                    &Default::default(),
                )?;
                let interrupted = crate::scheduling::JobOutcome::Interrupted {
                    detail: "lost".into(),
                    process_unconfirmed: true,
                };
                store.finish_job(
                    &node.identity,
                    &network,
                    "c",
                    &trigger,
                    &attempt,
                    interrupted,
                    "",
                    now_ms(),
                )?;
            }
            node.reconcile_checks(&network).await?;
            let left = node.store.lock().await.pending_checks()?;
            let servers: Vec<_> = left
                .iter()
                .filter_map(|check| check.codex_turn.as_ref())
                .map(|turn| turn.server.as_str())
                .collect();
            assert_eq!(
                servers,
                ["own:"],
                "only the turn on this App Server was found gone"
            );
            Ok::<_, anyhow::Error>(())
        }
        .await;
        cleanup(node, dir).await?;
        result
    }

    #[tokio::test]
    async fn a_bots_reply_thread_is_its_chats_and_takes_no_work() -> Result<()> {
        let (node, admin, dir) = node().await?;
        let result = async {
            with_agent(&node);
            bot(&node, &admin, "c", "前端", &[]).await?;
            let network = node.network_id().await?;
            let (trigger, attempt) = start_reply(&node, &admin, "m1").await?;
            node.store.lock().await.save_scheduler_native_id(
                "c",
                &trigger,
                &attempt,
                "bot-thread",
                "token",
                false,
            )?;
            let event = node
                .ingest_agent_event(turn("bot-thread", "t1", "/w"))
                .await?;
            assert_eq!(event["status"], "own");
            assert!(
                unanswered(&node).await?.is_empty(),
                "no question about a bot's own thread"
            );
            assert!(
                node.pending_events().await?.is_empty(),
                "a bot's own turn does not wake the Agent"
            );
            let held = AgentEvent {
                request_id: Some("r1".into()),
                ..turn("bot-thread", "request:r1", "/w")
            };
            assert_eq!(node.queue_event(held).await?["status"], "accepted");

            let access = Access {
                network_id: network.clone(),
                human: false,
                conversation_id: Some("c".into()),
                manager: None,
            };
            node.register_manager_attempt("token".into(), access.clone(), &attempt, &trigger)
                .await?;
            let access = Access {
                manager: Some(crate::runtime::ManagerAccess {
                    attempt_id: attempt.clone(),
                    trigger,
                }),
                ..access
            };
            let args = json!({"thread":"bot-thread","call_ref":"r1","title":"t","prompt":"p"});
            let refused = node.tool(access, "track_work", args).await;
            node.finish_manager_attempt(&attempt).await;
            let refused = refused.expect_err("work into a bot's reply thread");
            assert!(
                refused.to_string().contains("own reply thread"),
                "{refused:#}"
            );
            Ok::<_, anyhow::Error>(())
        }
        .await;
        cleanup(node, dir).await?;
        result
    }

    #[tokio::test]
    async fn the_coordination_agents_thread_is_not_triaged() -> Result<()> {
        use futures_util::{SinkExt as _, StreamExt as _};
        let (node, admin, dir) = node().await?;
        let result = async {
            with_agent(&node);
            bot(&node, &admin, "c", "前端", &[]).await?;
            let listener = tokio::net::UnixListener::bind(named_socket(&node))?;
            tokio::spawn(async move {
                while let Ok((stream, _)) = listener.accept().await {
                    tokio::spawn(async move {
                        let Ok(mut server) = tokio_tungstenite::accept_async(stream).await else {
                            return;
                        };
                        while let Some(Ok(message)) = server.next().await {
                            let Ok(request) = serde_json::from_slice::<Value>(&message.into_data())
                            else {
                                continue;
                            };
                            if request.get("id").is_none() {
                                continue;
                            }
                            let result = match request["method"].as_str() {
                                Some("initialize") => json!({"userAgent":"codex/fixture"}),
                                Some("thread/start") => json!({"thread":{"id":"coord-thread"}}),
                                Some("turn/start") => json!({"turn":{"id":"coord-turn"}}),
                                _ => json!({}),
                            };
                            let mut replies = vec![json!({"id":request["id"],"result":result})];
                            if request["method"] == "turn/start" {
                                replies.push(json!({"method":"turn/completed","params":{
                                    "threadId":"coord-thread",
                                    "turn":{"id":"coord-turn","status":"completed"}}}));
                            }
                            for reply in replies {
                                let _ = server.send(reply.to_string().into()).await;
                            }
                        }
                    });
                }
            });
            node.coordination_turn(|_| Ok("triage".into())).await?;
            *node.triage.config.lock().unwrap() = TriageConfig {
                scan_codex_sessions: false,
                ..Default::default()
            };
            node.scan_sources().await?;

            let ended = node
                .ingest_agent_event(turn("coord-thread", "coord-turn", "/tmp"))
                .await?;
            assert_eq!(ended["status"], "own");
            let held = AgentEvent {
                request_id: Some("r1".into()),
                ..turn("coord-thread", "request:r1", "/tmp")
            };
            assert_eq!(node.queue_event(held).await?["status"], "own");
            assert!(unanswered(&node).await?.is_empty(), "no question about it");
            assert!(node.pending_events().await?.is_empty(), "nothing queued");
            assert_eq!(
                owner_of(&node, "coord-thread").await?,
                None,
                "no bot is given it"
            );
            Ok::<_, anyhow::Error>(())
        }
        .await;
        cleanup(node, dir).await?;
        result
    }

    #[tokio::test]
    async fn events_dedupe_and_questions_go_once_to_the_owner_and_signed_webhooks() -> Result<()> {
        let (node, admin, dir) = node().await?;
        let received = Arc::new(std::sync::Mutex::new(
            Vec::<(axum::http::HeaderMap, String)>::new(),
        ));
        let sink = received.clone();
        let log = received.clone();
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
            bot(&node, &admin, "c", "前端", &["s1", "s2"]).await?;
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
            let raise = json!({"action":"raise","event_id":event_id,"severity":"urgent","question":"测试失败，要修吗？","choices":["修","不修"]});
            let raised = node.triage_command(raise.clone()).await?;
            assert_eq!((raised["asked"].clone(), raised["queued_webhook"].clone()), (json!(true), json!(true)));
            assert_eq!(node.triage_command(raise).await?, raised, "a repeated raise answers the same");
            let pending = node.triage_command(json!({"action":"events"})).await?;
            let rest = |session: &str| {
                pending["events"].as_array().unwrap().iter().find(|e| e["session_id"] == session).unwrap()["event_id"].clone()
            };
            let same_session = node.triage_command(json!({"action":"raise","event_id":rest("s1"),"severity":"info","question":"again"})).await?;
            assert_eq!((same_session["asked"].clone(), same_session["question_id"].clone()), (json!(false), raised["question_id"].clone()),
                "an unanswered question about the session stands");
            let quiet = node.triage_command(json!({"action":"dismiss","event_id":rest("s2")})).await?;
            assert_eq!(quiet["dismissed"], true);
            assert!(node.triage_command(json!({"action":"raise","event_id":"unknown","severity":"info","question":"x"})).await.is_err());
            assert_eq!(node.triage_command(json!({"action":"events"})).await?["events"], json!([]));
            let asked = unanswered(&node).await?;
            assert_eq!(asked.len(), 1);
            assert_eq!((asked[0]["severity"].clone(), asked[0]["text"].clone(), asked[0]["conversation_id"].clone()),
                (json!("urgent"), json!("测试失败，要修吗？"), json!("c")));

            node.deliver_webhooks().await?;
            node.deliver_webhooks().await?;
            let question = raised["question_id"].as_str().unwrap();
            assert_eq!(answer(&node, question, "修").await?, "answered");
            node.send_answered().await?;
            node.send_answered().await?;
            node.deliver_webhooks().await?;
            let received = received.lock().unwrap().clone();
            assert_eq!(received.len(), 2, "the question and its answer, once each");
            let (headers, body) = &received[0];
            let timestamp: i64 = headers["x-vibeface-timestamp"].to_str()?.parse()?;
            assert_eq!(headers["x-vibeface-event-id"], question);
            assert_eq!(headers["x-vibeface-signature"].to_str()?, triage::signature(secret.as_bytes(), timestamp, body));
            let body: Value = serde_json::from_str(body)?;
            assert_eq!((body["type"].clone(), body["choices"][1].clone()), (json!("question"), json!({"index":1,"label":"不修"})));
            assert_eq!(body["answer"], json!({"url":node.decision_url(),"device_id":node.identity.device_id()?,"question_id":question}));
            let answered: Value = serde_json::from_str(&received[1].1)?;
            let network = node.network_id().await?;
            let hash = node.store.lock().await.message(&network, "c", &format!("a-{question}"))?.context("answer")?.event_hash;
            assert_eq!(answered, json!({"schema_version":1,"type":"answered","event_id":format!("answered-{question}-{}", &hash[..16]),
                "question_id":question,"text":"修"}));
            assert_eq!(node.triage_settings().await?["outbox"]["delivered"], 2);

            let winner = "0".repeat(64);
            node.store.lock().await.connection.execute(
                "UPDATE messages SET text='不修',event_hash=?1 WHERE network_id=?2 AND message_id=?3",
                params![winner, network, format!("a-{question}")],
            )?;
            node.send_answered().await?;
            node.send_answered().await?;
            node.deliver_webhooks().await?;
            let received = log.lock().unwrap().clone();
            assert_eq!(received.len(), 3, "the winning answer follows once");
            let corrected: Value = serde_json::from_str(&received[2].1)?;
            assert_eq!((corrected["event_id"].clone(), corrected["text"].clone()),
                (json!(format!("answered-{question}-{}", &winner[..16])), json!("不修")));
            assert_eq!(node.test_webhook().await?["ok"], true);
            Ok::<_, anyhow::Error>(())
        }
        .await;
        server.abort();
        drop(node);
        std::fs::remove_dir_all(dir)?;
        result
    }

    #[tokio::test]
    async fn decision_tokens_answer_questions_once_and_are_revocable() -> Result<()> {
        let (node, admin, dir) = node().await?;
        let result = async {
            with_agent(&node);
            bot(&node, &admin, "c", "前端", &["s1"]).await?;
            let created = node.create_decision_token("grok").await?;
            let token = created["token"].as_str().unwrap().to_owned();
            let stored = std::fs::read_to_string(dir.join(DECISION_TOKENS))?;
            assert!(
                !stored.contains(&token)
                    && stored.contains(&crate::crypto::digest(token.as_bytes()))
            );
            assert!(!node.triage_settings().await?.to_string().contains(&token));
            assert!(node.decision_token_valid(&token) && !node.decision_token_valid("vfd_wrong"));
            let event = node.ingest_agent_event(turn("s1", "t1", "/w")).await?;
            let raised = node
                .triage_command(json!({"action":"raise","event_id":event["event_id"],
                    "severity":"attention","question":"要修吗？","choices":["修","不修"]}))
                .await?;
            let device = node.identity.device_id()?;
            let question = raised["question_id"].as_str().unwrap().to_owned();
            let decision = json!({"device_id":device,"question_id":question,"choice":0});
            let refused = node
                .decision_http("vfd_wrong", decision.clone())
                .await
                .unwrap_err();
            assert!(format!("{refused:#}").contains("invalid decision token"));
            let answered = node.decision_http(&token, decision.clone()).await?;
            assert_eq!(
                answered,
                json!({"question_id":question,"answer_id":format!("a-{question}"),
                "status":"answered","text":"修"})
            );
            assert_eq!(
                node.decision_http(&token, decision).await?,
                answered,
                "a retry reads back"
            );
            let other = node
                .decision_http(
                    &token,
                    json!({"device_id":device,"question_id":question,"text":"不修"}),
                )
                .await?;
            assert_eq!(
                (other["status"].clone(), other["text"].clone()),
                (json!("already_answered"), json!("修"))
            );
            assert!(unanswered(&node).await?.is_empty());
            node.revoke_decision_token(created["token_id"].as_str().unwrap())
                .await?;
            assert!(!node.decision_token_valid(&token));
            Ok::<_, anyhow::Error>(())
        }
        .await;
        cleanup(node, dir).await?;
        result
    }
}
