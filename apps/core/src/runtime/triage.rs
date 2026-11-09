//! Triage workers (design 4): capture into one queue, hand batches to the coordination Agent,
//! ask the user in the chat of the bot that owns each session, deliver webhooks from a
//! local outbox, hold the requests sessions wait on for the Agent, and give sessions no bot
//! started to the primary bot until it hands them to another or stops following them.
use super::{Access, Changed, Runtime, now_ms};
use crate::{
    config,
    manager::{self, CursorInstance, ManagerConfig},
    model::{AttentionKind, AttentionRequest, Choice, EventBody, Subject},
    net,
    providers::Provider,
    store::triage::{BudgetStatus, TriageAdmission},
    triage::{
        self, AgentEvent, AnswerAuthority, AnswerResponse, Delivery, HeldAnswerControl,
        HeldAnswerRequest, HookRequest, OutboxEntry, RequestKind, Severity, TriageConfig,
    },
};
use anyhow::{Context as _, Result, bail, ensure};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::{Value, json};
use std::{
    collections::{HashMap, HashSet, hash_map::Entry},
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};
use vibeface_bridge::codex::ServerRequest;

const WAKE_EVERY: Duration = Duration::from_secs(20);
pub const TRIAGE_URL_ENV: &str = "VIBEFACE_TRIAGE_URL";
const BATCH: usize = 20;
/// A batch can name up to `BATCH` bots, so each one's part of the prompt stays small.
const BOT_INSTRUCTIONS_BYTES: usize = 1000;
const BOT_NOTES_BYTES: usize = 1000;
const SCAN_EVERY: Duration = Duration::from_secs(300);
const SCAN_WINDOW_MS: i64 = 24 * 3600 * 1000;
const SEEN_TTL_MS: i64 = 7 * 24 * 3600 * 1000;
/// The session of an App Server request that belongs to no thread.
const NO_THREAD: &str = "app-server";
/// How long a held App Server request waits for the coordination Agent before the fallback answer.
pub const AGENT_ANSWER_WITHIN: Duration = Duration::from_secs(120);
/// How long a hook's held request waits; its session sits blocked in front of the user meanwhile.
pub const HOOK_ANSWER_WITHIN: Duration = Duration::from_secs(30);
const WEBHOOK_KEY: &str = "triage-webhook.key";
const DECISION_TOKENS: &str = "decision-tokens.json";
pub const SPOOL_DIR: &str = "hook-spool";

/// Where a webhook's reader answers. Without a public HTTPS URL, a Node on the public bootstrap
/// is answered through the official public node, which hands each decision to it by device ID.
fn decision_url(config: &config::Config) -> Result<String> {
    let base = if config.public_url.starts_with("https://")
        || config.network.bootstrap != net::bootstrap::Bootstrap::Public
    {
        config.public_url.clone()
    } else {
        net::bootstrap::PublicDefaults::load()?.gateway_url
    };
    Ok(format!("{}/api/decision", base.trim_end_matches('/')))
}

pub(crate) struct TriageState {
    config: std::sync::Mutex<TriageConfig>,
    /// Requests sessions hold until they are answered, by the Node's own request ID. An App
    /// Server request lives only as long as the connection that carried it.
    requests: std::sync::Mutex<HashMap<String, HeldRequest>>,
    /// Answers given in this process: answering again returns the first answer.
    answered: std::sync::Mutex<HashMap<String, Value>>,
    wake: tokio::sync::Notify,
    outbox: tokio::sync::Notify,
    /// App Server threads' working directory and last answer, for their turn-end events.
    threads: std::sync::Mutex<HashMap<String, (String, String)>>,
    /// App Server threads nobody drives (`codex exec`, sub-agents); no bot is given them.
    background: std::sync::Mutex<HashSet<String>>,
    started_ms: i64,
    pub(super) turn: tokio::sync::Mutex<()>,
    active: std::sync::Mutex<Option<String>>,
    budget_warning: std::sync::Mutex<Option<BudgetStatus>>,
    mutations: tokio::sync::Mutex<()>,
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
            turn: Default::default(),
            active: Default::default(),
            budget_warning: Default::default(),
            mutations: Default::default(),
        }
    }
    pub(super) fn config(&self) -> TriageConfig {
        self.config
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
    pub(super) fn set_config(&self, config: TriageConfig) {
        *self
            .config
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = config;
        self.outbox.notify_one();
    }
}

struct ActiveTriage<'a>(&'a TriageState);

impl Drop for ActiveTriage<'_> {
    fn drop(&mut self) {
        *lock(&self.0.active) = None;
    }
}

/// A request a session holds until it is answered.
#[derive(Clone, Serialize)]
struct HeldRequest {
    request_id: String,
    provider: Provider,
    /// The thread or session that asked.
    session_id: String,
    #[serde(skip)]
    session_turn: String,
    #[serde(skip)]
    generation: String,
    #[serde(skip)]
    permission_epoch: u64,
    #[serde(skip)]
    owner_epoch: Option<u64>,
    #[serde(skip)]
    response: AnswerResponse,
    #[serde(skip)]
    answer_manager: Option<ManagerConfig>,
    kind: RequestKind,
    received_ms: i64,
    /// `None` while a question holds it for the user. A hook's stays: the hook waits only so long.
    answer_by_ms: Option<i64>,
    question_id: Option<String>,
    fallback_reason: Option<&'static str>,
    #[serde(flatten)]
    reply: Reply,
}

/// Where a held request's answer goes.
#[derive(Clone, Serialize)]
#[serde(untagged)]
enum Reply {
    /// The JSON-RPC answer on the App Server connection that asked.
    AppServer {
        #[serde(skip)]
        rpc_id: Value,
        method: String,
        turn_id: Option<String>,
        params: Value,
    },
    /// The hook waiting on `/api/hooks/wait`.
    Hook {
        #[serde(skip)]
        answer: tokio::sync::watch::Sender<Option<Value>>,
        /// A hook came to wait; once none waits any more, its session no longer takes an answer.
        #[serde(skip)]
        waited: bool,
    },
}

impl HeldRequest {
    fn answer_binding(&self, node: &str) -> HeldAnswerRequest {
        HeldAnswerRequest {
            node: node.into(),
            request_id: self.request_id.clone(),
            provider: self.provider,
            session_id: self.session_id.clone(),
            turn_id: self.session_turn.clone(),
            transport: self.origin().into(),
            generation: self.generation.clone(),
            permission_epoch: self.permission_epoch,
            owner_epoch: self.owner_epoch,
            response: self.response.clone(),
        }
    }
    /// What asked, for logs: the App Server method, or a hook.
    fn origin(&self) -> &str {
        match &self.reply {
            Reply::AppServer { method, .. } => method,
            Reply::Hook { .. } => "hook",
        }
    }
}

/// Keeps how a held request was answered, so a second answer and its hook's wait find it.
fn remember(
    answered: &mut HashMap<String, Value>,
    request: &HeldRequest,
    request_id: &str,
    decision: &str,
    by: &str,
    sent: Result<(), String>,
    answer: Option<Value>,
) -> Value {
    let mut outcome = json!({"request_id":request_id,"provider":request.provider,
        "session_id":request.session_id,"kind":request.kind,"decision":decision,"by":by,
        "answered":sent.is_ok(),"error":sent.as_ref().err()});
    if let Some(answer) = answer {
        outcome["answer"] = answer;
    }
    tracing::info!(event = "request.answered", request = %request_id, origin = request.origin(), %decision, %by,
        answered = sent.is_ok(), "held request answered");
    // ponytail: forget answers past 1024; a restart forgets them all, as the requests.
    if answered.len() > 1024 {
        answered.clear();
    }
    answered.insert(request_id.into(), outcome.clone());
    outcome
}

/// What the hook prints when nobody answered: its session decides.
fn undecided() -> Value {
    json!({"decision":null})
}

/// Approvals ask permission; every other App Server request asks a question.
fn codex_kind(method: &str) -> RequestKind {
    match method.ends_with("Approval") {
        true => RequestKind::Permission,
        false => RequestKind::Question,
    }
}

fn codex_answer_response(method: &str, params: &Value) -> AnswerResponse {
    let approval = |allow_once, deny| AnswerResponse::Approval { allow_once, deny };
    let decision = |allow: &str, deny: &str, cancel: &str| {
        let offered = |value: &str| match params.get("availableDecisions") {
            None => true,
            Some(Value::Array(values)) => values
                .iter()
                .any(|candidate| candidate.as_str() == Some(value)),
            Some(_) => false,
        };
        let deny = if offered(deny) {
            deny
        } else if offered(cancel) {
            cancel
        } else {
            return AnswerResponse::Unsupported;
        };
        approval(
            offered(allow).then(|| json!({"decision":allow})),
            json!({"decision":deny}),
        )
    };
    match method {
        "item/commandExecution/requestApproval" | "item/fileChange/requestApproval" => {
            decision("accept", "decline", "cancel")
        }
        "execCommandApproval" | "applyPatchApproval" => decision("approved", "denied", "abort"),
        "item/permissions/requestApproval" => {
            approval(None, json!({"permissions":{},"scope":"turn"}))
        }
        "item/tool/requestUserInput" if params["questions"].is_array() => {
            AnswerResponse::CodexQuestion {
                questions: params["questions"].clone(),
            }
        }
        _ => AnswerResponse::Unsupported,
    }
}

fn hook_answer_response(
    event: &AgentEvent,
    schema: Option<&HookRequest>,
) -> Result<AnswerResponse> {
    if let Some(schema) = schema {
        ensure!(
            schema.provider == event.agent
                && schema.session_id == event.session_id
                && schema.turn_id == event.turn_id
                && Some(schema.kind) == event.hold,
            "hook request metadata does not belong to this event"
        );
    }
    Ok(match (event.agent, event.hold) {
        (Provider::Claude, Some(RequestKind::Permission)) => AnswerResponse::Approval {
            allow_once: Some(json!({"behavior":"allow"})),
            deny: json!({"behavior":"deny"}),
        },
        (Provider::Opencode, Some(RequestKind::Permission)) => AnswerResponse::Approval {
            allow_once: Some(json!({"reply":"once"})),
            deny: json!({"reply":"reject"}),
        },
        (Provider::Claude, Some(RequestKind::Question)) => match schema
            .and_then(|s| s.questions.clone())
        {
            Some(questions) if questions.is_array() => AnswerResponse::ClaudeQuestion { questions },
            _ => AnswerResponse::Unsupported,
        },
        (Provider::Opencode, Some(RequestKind::Question)) => {
            match schema.and_then(|s| s.questions.clone()) {
                Some(questions) if questions.is_array() => {
                    AnswerResponse::OpencodeQuestion { questions }
                }
                _ => AnswerResponse::Unsupported,
            }
        }
        (Provider::Pi, Some(RequestKind::Continue)) => AnswerResponse::Continue,
        _ => AnswerResponse::Unsupported,
    })
}

fn validate_answer_result(
    response: &AnswerResponse,
    question: &Value,
    text: &str,
    result: &Value,
) -> Result<()> {
    if let Some(expected) = crate::store::triage::approval_answer_result(question, text, response)?
    {
        ensure!(
            result == &expected,
            "response differs from the user's approval action"
        );
        return Ok(());
    }
    let explicit = serde_json::from_str::<Value>(text)
        .ok()
        .is_some_and(|value| &value == result);
    let only = |value: &Value, field: &str| {
        value
            .as_object()
            .is_some_and(|fields| fields.len() == 1 && fields.contains_key(field))
    };
    let selection = |q: &Value, answers: &[Value], custom: bool, multi: bool| -> Result<()> {
        ensure!(
            !answers.is_empty() && (multi || answers.len() == 1),
            "invalid answer count"
        );
        for answer in answers {
            let answer = answer.as_str().context("answers must be strings")?;
            ensure!(!answer.is_empty(), "an answer cannot be empty");
            let offered = q["options"]
                .as_array()
                .into_iter()
                .flatten()
                .any(|option| option["label"].as_str() == Some(answer));
            ensure!(
                offered || custom,
                "answer is not one of the original request's choices"
            );
            ensure!(
                explicit || answer == text,
                "text answers must preserve the user's original answer"
            );
        }
        Ok(())
    };
    match response {
        AnswerResponse::Continue => ensure!(
            result == &json!({"text":text}),
            "continuation must use the user's original text"
        ),
        AnswerResponse::CodexQuestion { questions } => {
            ensure!(only(result, "answers"), "invalid requestUserInput response");
            let questions = questions
                .as_array()
                .context("invalid original question schema")?;
            let answers = result["answers"]
                .as_object()
                .context("answers must be an object")?;
            ensure!(
                !questions.is_empty() && questions.len() == answers.len(),
                "every original question needs one answer"
            );
            for q in questions {
                let id = q["id"].as_str().context("original question has no ID")?;
                let answer = answers
                    .get(id)
                    .context("answer uses another question's ID")?;
                ensure!(only(answer, "answers"), "invalid question answer");
                let values = answer["answers"]
                    .as_array()
                    .context("question answers must be an array")?;
                let custom =
                    q["options"].as_array().is_none_or(Vec::is_empty) || q["isOther"] == true;
                selection(q, values, custom, false)?;
            }
        }
        AnswerResponse::ClaudeQuestion { questions } => {
            ensure!(only(result, "answers"), "invalid AskUserQuestion response");
            let questions = questions
                .as_array()
                .context("invalid original question schema")?;
            let answers = result["answers"]
                .as_object()
                .context("answers must be an object")?;
            ensure!(
                !questions.is_empty() && questions.len() == answers.len(),
                "every original question needs one answer"
            );
            for q in questions {
                let key = q["question"]
                    .as_str()
                    .context("original question has no text")?;
                let answer = answers
                    .get(key)
                    .context("answer uses another question's text")?;
                selection(q, std::slice::from_ref(answer), true, false)?;
            }
        }
        AnswerResponse::OpencodeQuestion { questions } => {
            ensure!(
                only(result, "answers"),
                "invalid OpenCode question response"
            );
            let questions = questions
                .as_array()
                .context("invalid original question schema")?;
            let answers = result["answers"]
                .as_array()
                .context("answers must be an array")?;
            ensure!(
                !questions.is_empty() && questions.len() == answers.len(),
                "every original question needs one answer"
            );
            for (q, answer) in questions.iter().zip(answers) {
                let values = answer
                    .as_array()
                    .context("each question answer must be an array")?;
                selection(q, values, q["custom"] != false, q["multiple"] == true)?;
            }
        }
        AnswerResponse::Approval { .. } => unreachable!("approval was checked above"),
        AnswerResponse::Unsupported => bail!("the original request has no supported answer schema"),
    }
    Ok(())
}

fn lock<T>(mutex: &std::sync::Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// What a request asks, for the event the Agent and the outlets see. Commands, patches and
/// paths are left out; the Agent reads them with `vibeface requests`.
fn describe(method: &str, params: &Value) -> String {
    let questions = crate::triage::question_lines(&params["questions"]);
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

/// One line of what a session last said. A held request's `detail` tells the coordination Agent
/// how to answer, so the person never sees it.
fn summary(event: &AgentEvent) -> String {
    let detail = if event.hold.is_some() {
        ""
    } else {
        event.detail.as_str()
    };
    [event.last_message.as_str(), detail]
        .into_iter()
        .flat_map(|text| text.lines())
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or_default()
        .chars()
        .take(120)
        .collect()
}

fn subject(event: &AgentEvent, request_id: Option<String>) -> Subject {
    Subject {
        node: event.node.clone(),
        agent: Some(event.agent),
        session_id: event.session_id.clone(),
        cwd: event.cwd.clone(),
        summary: summary(event),
        request_id,
    }
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

/// The bot that owns this device's session. A thread this device's work bound counts at
/// once: the App Server can ask before the turn's start is recorded. A bot's own reply thread
/// belongs to its conversation. A deleted bot owns neither, so they go to the primary bot.
fn owner(store: &crate::store::Store, network: &str, event: &AgentEvent) -> Result<Option<String>> {
    if let Some(owner) = store.session_owner(network, &event.node, &event.session_id)? {
        return Ok(Some(owner));
    }
    let bound = match store.native_thread_conversation(&event.session_id)? {
        Some(conversation) => Some(conversation),
        None => store
            .connection
            .query_row(
                "SELECT conversation_id FROM work_runs WHERE thread_id=? ORDER BY work_id LIMIT 1",
                [&event.session_id],
                |r| r.get(0),
            )
            .optional()?,
    };
    Ok(match bound {
        Some(conversation) if !store.bot_deleted(network, &conversation)? => Some(conversation),
        _ => None,
    })
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

/// What the coordination Agent knows of a bot: the start of its instructions and its newest
/// notes that fit, each within its own byte budget.
fn bot_context(bot: &crate::model::Bot, notes: &[crate::model::MemoryEntry]) -> Value {
    let mut left = BOT_NOTES_BYTES;
    let kept: Vec<&str> = notes
        .iter()
        .rev()
        .map(|note| note.text.as_str())
        .filter(|text| {
            let fits = text.len() <= left;
            if fits {
                left -= text.len();
            }
            fits
        })
        .collect();
    json!({"conversation_id":bot.conversation_id,"name":bot.name,
        "instructions":crate::scheduler::prefix(&bot.instructions, BOT_INSTRUCTIONS_BYTES),
        "memory":kept,"memory_omitted":notes.len() - kept.len()})
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
        self.ingest_hook_event(event, None).await
    }

    pub(crate) async fn ingest_hook_event(
        &self,
        event: AgentEvent,
        schema: Option<HookRequest>,
    ) -> Result<Value> {
        let event = AgentEvent {
            request_id: None,
            ..event
        };
        match event.hold {
            Some(kind) => self.hold_request(event, kind, schema).await,
            None => self.queue_event(event).await,
        }
    }

    /// A hook's event whose session waits for the answer: held like an App Server request, and
    /// answered to the hook waiting on `/api/hooks/wait`. An event that is not queued holds
    /// nothing, and neither does a second one for a request already held; their hooks leave
    /// the decision to the session.
    async fn hold_request(
        &self,
        event: AgentEvent,
        kind: RequestKind,
        schema: Option<HookRequest>,
    ) -> Result<Value> {
        let response = hook_answer_response(&event, schema.as_ref())?;
        let request_id = AgentEvent::id_for(
            &self.identity.device_id()?,
            event.agent,
            &event.session_id,
            &event.turn_id,
            event.event,
        );
        let now = now_ms();
        let request = HeldRequest {
            request_id: request_id.clone(),
            provider: event.agent,
            session_id: event.session_id.clone(),
            session_turn: event.turn_id.clone(),
            generation: uuid::Uuid::new_v4().simple().to_string(),
            permission_epoch: self
                .local_permission_epoch
                .load(std::sync::atomic::Ordering::Acquire),
            owner_epoch: None,
            response,
            answer_manager: self.capture_answer_manager().ok().flatten(),
            kind,
            received_ms: now,
            answer_by_ms: Some(now + HOOK_ANSWER_WITHIN.as_millis() as i64),
            question_id: None,
            fallback_reason: None,
            reply: Reply::Hook {
                answer: tokio::sync::watch::channel(None).0,
                waited: false,
            },
        };
        match lock(&self.triage.requests).entry(request_id.clone()) {
            Entry::Occupied(_) => return Ok(json!({"status":"duplicate","event_id":request_id})),
            Entry::Vacant(slot) => slot.insert(request),
        };
        let queued = self
            .queue_event(AgentEvent {
                request_id: Some(request_id.clone()),
                ..event
            })
            .await;
        match queued {
            Ok(mut result) if result["status"] == "accepted" && result["test"] != true => {
                tracing::info!(event = "request.held", request = %request_id, provider = event.agent.name(), ?kind,
                    "hook request held");
                result["request_id"] = json!(request_id);
                self.triage.wake.notify_one();
                Ok(result)
            }
            queued => {
                lock(&self.triage.requests).remove(&request_id);
                queued
            }
        }
    }

    /// `/api/hooks/wait`: the answer to a hook's held request once it is given, or
    /// `{"decision":null}` at its deadline or when nothing is held, so the hook leaves the
    /// decision to its session.
    pub(crate) async fn wait_request(&self, request_id: &str) -> Value {
        let held = match lock(&self.triage.requests).get_mut(request_id) {
            Some(HeldRequest {
                reply: Reply::Hook { answer, waited },
                answer_by_ms: Some(by),
                ..
            }) => {
                *waited = true;
                Some((answer.subscribe(), *by))
            }
            _ => None,
        };
        let Some((mut answer, by)) = held else {
            // Answered before its hook came to wait.
            return lock(&self.triage.answered)
                .get(request_id)
                .and_then(|done| done.get("answer").cloned())
                .unwrap_or_else(undecided);
        };
        let left = Duration::from_millis(u64::try_from(by - now_ms()).unwrap_or(0));
        let answered = tokio::time::timeout(left, answer.wait_for(Option::is_some))
            .await
            .is_ok();
        if !answered {
            let _ = self.answer_request(request_id, None, "fallback").await;
        }
        let answer = answer.borrow().clone();
        answer.unwrap_or_else(undecided)
    }

    async fn queue_event(&self, event: AgentEvent) -> Result<Value> {
        crate::logging::add_private_path(Path::new(&event.cwd));
        let event = event.seal(&self.identity.device_id()?, config::UNNAMED_NODE, now_ms())?;
        if event.test {
            tracing::info!(
                event = "triage.test_received",
                event_id = event.event_id,
                "triage test event received"
            );
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
        let scope = match owner(&store, &network, &event)? {
            Some(scope) => Some(scope),
            None => self.claim_session(&mut store, &network, &event)?,
        };
        let Some(scope) = scope else {
            return Ok(json!({"status":"foreign","event_id":event.event_id}));
        };
        if let Some(id) = &event.request_id {
            let epoch = store
                .conversation(&network, &scope)?
                .map(|conversation| conversation.owner_epoch);
            if let Some(request) = lock(&self.triage.requests).get_mut(id) {
                request.owner_epoch = epoch;
            }
        }
        let event_id = event.event_id.clone();
        if !store.queue_triage_event(event, now_ms())? {
            return Ok(json!({"status":"duplicate","event_id":event_id}));
        }
        Ok(json!({"status":"accepted","event_id":event_id}))
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
            &subject(&shared_event(event, &self.settings_snapshot().roots), None),
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
    /// the fallback at once when there is none or no bot owns its thread. The same request asked
    /// again, as after a reconnect, keeps its ID: it takes the new connection's JSON-RPC ID and
    /// stays with the question or the Agent event it already has, so nothing is asked twice.
    pub(crate) async fn on_codex_request(self: &Arc<Self>, handed: &ServerRequest) {
        let request_id = triage::request_id(&handed.method, &handed.params)
            .unwrap_or_else(|| uuid::Uuid::new_v4().simple().to_string());
        let mut request = HeldRequest {
            request_id: request_id.clone(),
            provider: Provider::Codex,
            session_id: handed.thread_id.clone().unwrap_or(NO_THREAD.into()),
            session_turn: handed.params["turnId"].as_str().unwrap_or_default().into(),
            generation: uuid::Uuid::new_v4().simple().to_string(),
            permission_epoch: self
                .local_permission_epoch
                .load(std::sync::atomic::Ordering::Acquire),
            owner_epoch: None,
            response: codex_answer_response(&handed.method, &handed.params),
            answer_manager: self.capture_answer_manager().ok().flatten(),
            kind: codex_kind(&handed.method),
            received_ms: now_ms(),
            answer_by_ms: Some(now_ms() + AGENT_ANSWER_WITHIN.as_millis() as i64),
            question_id: None,
            fallback_reason: None,
            reply: Reply::AppServer {
                rpc_id: json!(handed.id),
                method: handed.method.clone(),
                turn_id: handed.params["turnId"].as_str().map(str::to_owned),
                params: handed.params.clone(),
            },
        };
        if let Some(previous) = lock(&self.triage.requests).get(&request_id) {
            request.received_ms = previous.received_ms;
            request.answer_by_ms = previous.answer_by_ms;
            request.fallback_reason = previous.fallback_reason;
            request.owner_epoch = previous.owner_epoch;
        }
        // Asked again, the App Server never got the earlier answer.
        lock(&self.triage.answered).remove(&request_id);
        let standing = async {
            let network = self.network_id().await?;
            let node = self.identity.device_id()?;
            self.store
                .lock()
                .await
                .request_question(&network, &node, &request_id)
        }
        .await
        .unwrap_or_else(|error| {
            tracing::warn!(event = "request.question_lookup_failed", %error, request = %request_id, "could not look up the request's question");
            None
        });
        if let Some(question) = standing {
            let closed = !question["closed"].is_null();
            tracing::info!(event = "request.asked_again", request = %request_id, method = %handed.method, closed,
                "App Server request asked again");
            if !closed {
                request.question_id = question["question_id"].as_str().map(str::to_owned);
                if question["answer"].is_null() {
                    request.answer_by_ms = None;
                }
            }
            lock(&self.triage.requests).insert(request_id.clone(), request);
            if closed {
                let _ = self.answer_request(&request_id, None, "fallback").await;
            }
            return;
        }
        tracing::info!(event = "request.received", request = %request_id, method = %handed.method,
            thread = ?handed.thread_id, "App Server request received");
        let session_id = request.session_id.clone();
        lock(&self.triage.requests).insert(request_id.clone(), request);
        // The observer only resumes work threads, so their folder comes from the dispatch.
        let mut cwd = lock(&self.triage.threads)
            .get(&session_id)
            .map(|thread| thread.0.clone())
            .unwrap_or_default();
        if cwd.is_empty()
            && let Some(thread) = &handed.thread_id
        {
            cwd = self
                .store
                .lock()
                .await
                .work_cwd_on_thread(thread)
                .ok()
                .flatten()
                .unwrap_or_default();
        }
        if cwd.is_empty() {
            cwd = handed.params["cwd"].as_str().unwrap_or_default().to_owned();
        }
        let event = AgentEvent {
            source: triage::Source::AppServer,
            agent: Provider::Codex,
            event: triage::Kind::NeedsInput,
            session_id,
            turn_id: format!("request:{request_id}"),
            cwd,
            detail: describe(&handed.method, &handed.params),
            request_id: Some(request_id.clone()),
            ..Default::default()
        };
        let queued = self.queue_event(event).await;
        let waiting = match &queued {
            Ok(result) if result["status"] == "accepted" => true,
            // Its event still waits for the Agent from the earlier connection.
            Ok(result) if result["status"] == "duplicate" => {
                let attached = self.store.lock().await.attach_triage_request(
                    result["event_id"].as_str().unwrap_or_default(),
                    &request_id,
                );
                if let Err(error) = &attached {
                    tracing::warn!(event = "request.unrouted", %error, request = %request_id,
                        "App Server request could not rejoin its pending event");
                }
                attached.unwrap_or(false)
            }
            _ => false,
        };
        if !waiting {
            if let Err(error) = &queued {
                tracing::warn!(event = "request.unrouted", %error, "App Server request could not reach the Agent");
            }
            let _ = self.answer_request(&request_id, None, "fallback").await;
            return;
        }
        self.triage.wake.notify_one();
    }

    /// Answers a held request once: `result` from the Agent or the user, or when `None` the
    /// App Server method's fallback, or no decision for a hook. Answering again returns the
    /// first answer.
    pub(crate) async fn answer_request(
        &self,
        request_id: &str,
        result: Option<Result<Value, Value>>,
        by: &str,
    ) -> Result<Value> {
        self.send_request_answer(request_id, result, by, None).await
    }

    async fn send_request_answer(
        &self,
        request_id: &str,
        result: Option<Result<Value, Value>>,
        by: &str,
        expected: Option<&HeldAnswerRequest>,
    ) -> Result<Value> {
        if let Some(done) = lock(&self.triage.answered).get(request_id) {
            ensure!(
                expected.is_none() || done["by"] == by,
                "the request was completed by another actor"
            );
            return Ok(done.clone());
        }
        let node = self.identity.device_id()?;
        let (request, result, decision, settled) = {
            let mut requests = lock(&self.triage.requests);
            let mut answered = lock(&self.triage.answered);
            if let Some(request) = requests.get(request_id) {
                ensure!(
                    by != "agent" || request.question_id.is_none(),
                    "this request needs the user's answer to its question"
                );
                if let Some(expected) = expected {
                    ensure!(
                        request.answer_binding(&node) == *expected,
                        "the original request's transport changed before delivery"
                    );
                }
            }
            let Some(request) = requests.remove(request_id) else {
                let done = answered
                    .get(request_id)
                    .context("no request is waiting with this ID")?;
                ensure!(
                    expected.is_none() || done["by"] == by,
                    "the request was completed by another actor"
                );
                return Ok(done.clone());
            };
            if result.is_none() {
                tracing::info!(event = "request.fallback", request = %request_id,
                    reason = request.fallback_reason.unwrap_or("unanswered"),
                    "held request reached its fallback");
            }
            let (result, decision) = match (result, &request.reply) {
                (Some(result), _) => {
                    let decision = result
                        .as_ref()
                        .ok()
                        .and_then(|r| r["decision"].as_str().or(r["action"].as_str()))
                        .unwrap_or(if result.is_ok() { "answered" } else { "error" })
                        .to_owned();
                    (result, decision)
                }
                (None, Reply::AppServer { method, .. }) => {
                    let (result, decision) = vibeface_bridge::methods::fallback(method);
                    (result, decision.to_owned())
                }
                (None, Reply::Hook { .. }) => (Ok(undecided()), "undecided".to_owned()),
            };
            // A hook is answered here at once, so its wait always finds the answer. An answer
            // given at the same time finds an App Server's while it is sent.
            let settled = match &request.reply {
                Reply::AppServer { .. } => {
                    let sending = json!({"request_id":request_id,"by":by,"sending":true});
                    answered.insert(request_id.into(), sending);
                    None
                }
                Reply::Hook {
                    answer: waiting,
                    waited,
                } => {
                    // Its hook left at the deadline, or when the user answered in the session.
                    let (sent, given) = if *waited && waiting.receiver_count() == 0 {
                        let left = "the session no longer waits for this request";
                        (Err(left.to_owned()), None)
                    } else {
                        let given = result.clone().unwrap_or_else(|_| undecided());
                        waiting.send_replace(Some(given.clone()));
                        (Ok(()), Some(given))
                    };
                    let outcome = remember(
                        &mut answered,
                        &request,
                        request_id,
                        &decision,
                        by,
                        sent,
                        given,
                    );
                    Some(outcome)
                }
            };
            (request, result, decision, settled)
        };
        let outcome = match (settled, &request.reply) {
            (Some(outcome), _) => outcome,
            (None, Reply::Hook { .. }) => unreachable!("a hook is answered with its removal"),
            (None, Reply::AppServer { rpc_id, .. }) => {
                let sent = (self.codex.client().respond(rpc_id, result, &decision).await)
                    .map_err(|error| error.message);
                let mut answered = lock(&self.triage.answered);
                remember(
                    &mut answered,
                    &request,
                    request_id,
                    &decision,
                    by,
                    sent,
                    None,
                )
            }
        };
        self.store
            .lock()
            .await
            .finish_triage_request(request_id, by != "fallback")?;
        Ok(outcome)
    }

    /// Requests past their deadline get the fallback. Standing answers only rejoin a transport
    /// this process still holds, never a recovered or historical session.
    async fn expire_requests(&self) -> Result<()> {
        self.queue_live_answers().await?;
        let now = now_ms();
        let requests: Vec<HeldRequest> = lock(&self.triage.requests).values().cloned().collect();
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
                self.answer_request(&request.request_id, None, "fallback")
                    .await?;
            }
        }
        Ok(())
    }

    async fn queue_live_answers(&self) -> Result<()> {
        let network = self.network_id().await?;
        let node = self.identity.device_id()?;
        let requests: Vec<_> = lock(&self.triage.requests).values().cloned().collect();
        for request in requests {
            let Some(question) = &request.question_id else {
                continue;
            };
            let binding = request.answer_binding(&node);
            let queued = {
                let mut store = self.store.lock().await;
                let Some(question_body) = store.question(&network, question)? else {
                    continue;
                };
                if question_body["answer"].is_null() {
                    continue;
                }
                let scope = question_body["conversation_id"]
                    .as_str()
                    .context("question has no conversation")?;
                if store
                    .held_answer_receipt(
                        &network,
                        scope,
                        &crate::store::questions::answer_id(question),
                    )?
                    .is_some()
                {
                    continue;
                }
                store.queue_answer_control(
                    &network,
                    question,
                    &binding,
                    AnswerAuthority::Signed,
                    now_ms(),
                )
            };
            match queued {
                Ok(true) => self.triage.wake.notify_one(),
                Ok(false) => {}
                Err(error) => {
                    tracing::warn!(event = "answer.control_unavailable", request = %request.request_id, %error,
                    "standing answer cannot control this held request")
                }
            }
        }
        Ok(())
    }

    pub(crate) async fn accept_held_answer_delivery(
        &self,
        network: &str,
        conversation: &str,
        message_id: &str,
    ) -> Result<Option<crate::scheduling::AdmissionReceipt>> {
        let node = self.identity.device_id()?;
        let mut store = self.store.lock().await;
        let Some(message) = store.message(network, conversation, message_id)? else {
            return Ok(None);
        };
        let Some(question_id) = &message.answers else {
            return Ok(None);
        };
        let question = store
            .question(network, question_id)?
            .context("answer question is missing")?;
        let Some(request_id) = question["about"]["request_id"].as_str() else {
            return Ok(None);
        };
        ensure!(
            question["about"]["node"] == node,
            "this Node does not hold the answer's request"
        );
        if let Some(receipt) = store.held_answer_receipt(network, conversation, message_id)? {
            return Ok(Some(receipt));
        }
        let request = lock(&self.triage.requests)
            .get(request_id)
            .cloned()
            .context("the original request is no longer waiting on this Node")?;
        ensure!(
            request.question_id.as_deref() == Some(question_id),
            "the held request belongs to another question"
        );
        store.queue_answer_control(
            network,
            question_id,
            &request.answer_binding(&node),
            AnswerAuthority::Signed,
            now_ms(),
        )?;
        let receipt = store
            .held_answer_receipt(network, conversation, message_id)?
            .context("answer control receipt was not saved")?;
        drop(store);
        self.triage.wake.notify_one();
        Ok(Some(receipt))
    }

    async fn validate_live_answer_control(
        &self,
        attempt: &str,
        control: &HeldAnswerControl,
    ) -> Result<HeldRequest> {
        ensure!(
            lock(&self.triage.active).as_deref() == Some(attempt),
            "answer control is no longer active"
        );
        let node = self.identity.device_id()?;
        ensure!(
            control.network == self.network_id().await? && control.request.node == node,
            "answer control belongs to another Node or network"
        );
        self.store
            .lock()
            .await
            .validate_answer_control_claim(&self.boot, attempt, control)?;
        let request = lock(&self.triage.requests)
            .get(&control.request.request_id)
            .cloned()
            .context("the original request is no longer waiting")?;
        ensure!(
            request.question_id.as_deref() == Some(control.question_id.as_str())
                && request.answer_binding(&node) == control.request,
            "the held request or transport changed"
        );
        Ok(request)
    }

    async fn answer_permission_lease(
        &self,
        control: &HeldAnswerControl,
    ) -> Result<Vec<tokio::sync::OwnedRwLockReadGuard<()>>> {
        let request = &control.request;
        ensure!(
            self.local_permission_epoch
                .load(std::sync::atomic::Ordering::Acquire)
                == request.permission_epoch,
            "the request's captured permissions were revoked"
        );
        let active_group = |store: &crate::store::Store| -> Result<Vec<crate::work::WorkRecord>> {
            let mut active: Vec<_> = store
                .work_records_on_thread(&request.session_id)?
                .into_iter()
                .filter(|record| !record.state.finished())
                .collect();
            active.sort_by(|left, right| left.work_id.cmp(&right.work_id));
            ensure!(!active.is_empty(), "the request has no active work");
            if active.len() > 1 {
                ensure!(
                    request.provider == Provider::Codex,
                    "the request has multiple native executions"
                );
                let first = store
                    .work(&control.network, &active[0].work_id)?
                    .context("the request's work is missing")?;
                for record in active.iter().skip(1) {
                    let work = store
                        .work(&control.network, &record.work_id)?
                        .context("the request's work is missing")?;
                    ensure!(
                        store.work_shares_native_execution(&control.network, &first, &work)?,
                        "the request spans different native executions or work scopes"
                    );
                }
            }
            Ok(active)
        };
        let (manager_attempt, managed, runs) = {
            let store = self.store.lock().await;
            let manager: Option<String> = store.connection.query_row(
                "SELECT attempt_id FROM manager_turns WHERE thread_id=?1 AND (?2='' OR turn_id=?2) ORDER BY rowid DESC LIMIT 1",
                params![request.session_id, request.turn_id], |row| row.get(0),
            ).optional()?;
            (
                manager,
                store
                    .native_thread_conversation(&request.session_id)?
                    .is_some(),
                store.work_records_on_thread(&request.session_id)?,
            )
        };
        if let Some(attempt) = manager_attempt {
            return Ok(vec![
                self.manager_input_lease(&attempt, request.provider).await?,
            ]);
        }
        ensure!(
            !managed,
            "the request's management turn is no longer active"
        );
        if runs.is_empty() {
            return Ok(Vec::new());
        }
        let active = {
            let store = self.store.lock().await;
            active_group(&store)?
        };
        let work_ids: Vec<_> = active.iter().map(|record| record.work_id.clone()).collect();
        let work_id = &work_ids[0];
        let mut leases = Vec::new();
        let parent = {
            let store = self.store.lock().await;
            let work = store
                .work(&control.network, work_id)?
                .context("the request's work is missing")?;
            if let Some(parent) = &work.parent {
                let parent = store
                    .work_record(parent)?
                    .context("the request's parent task is missing")?;
                ensure!(
                    parent.kind == crate::model::WorkKind::Task && !parent.state.finished(),
                    "the request's parent task is no longer active"
                );
                let job = store
                    .scheduler_job(
                        &parent.conversation,
                        &crate::scheduling::task_trigger(&parent.work_id),
                    )?
                    .context("the request's parent task job is missing")?;
                let attempt = job
                    .state
                    .attempt()
                    .context("the request's parent task attempt is missing")?
                    .id
                    .clone();
                let provider = job
                    .runtime
                    .as_ref()
                    .context("the request's parent task runtime is missing")?
                    .provider;
                Some((parent.work_id, attempt, provider))
            } else {
                None
            }
        };
        // Parent first, then stable Work order, matching cancellation's lock order.
        if let Some((_, attempt, provider)) = &parent {
            leases.push(self.manager_input_lease(attempt, *provider).await?);
        }
        for id in &work_ids {
            leases.push(self.work_input_lock(id).await.read_owned().await);
        }
        let cwd = {
            let store = self.store.lock().await;
            let work = store
                .work(&control.network, work_id)?
                .context("the request's work is missing")?;
            Self::validate_work_assignment_roots(&store, &control.network, &work.trigger)?;
            work.cwd
        };
        let actual_cwd = self
            .work_native_directory(request.provider, Some(&request.session_id), &cwd)
            .await?;
        let execution = if work_ids.len() > 1 {
            Some(
                self.codex_native_execution(&control.network, &request.session_id, &actual_cwd)
                    .await?
                    .context("the request's shared native turn is no longer active")?,
            )
        } else {
            None
        };
        let store = self.store.lock().await;
        store.authorize_node(&control.network, &request.node)?;
        let active = active_group(&store)?;
        ensure!(
            active
                .iter()
                .map(|record| &record.work_id)
                .eq(work_ids.iter()),
            "the request's active works changed before delivery"
        );
        if let Some(execution) = &execution {
            ensure!(
                (request.turn_id.is_empty() || request.turn_id == execution.turn_id)
                    && work_ids.iter().any(|id| execution.work_ids.contains(id)),
                "the request does not identify the shared native turn"
            );
        }
        let stops = store.stop_requests()?;
        for record in &active {
            let work_id = &record.work_id;
            ensure!(
                record.kind == crate::model::WorkKind::Agent(request.provider)
                    && !stops.iter().any(|stop| &stop.work_id == work_id)
                    && !store.peer_cancelled(&control.network, &record.conversation, work_id)?,
                "the request's work was stopped or changed provider"
            );
            ensure!(
                store.work_native_write_allowed(&control.network, work_id)?,
                "the request's work has no current authority for native submission"
            );
            if let Some(handle) = record.state.handle() {
                ensure!(
                    (request.turn_id.is_empty() || handle.turn_id == request.turn_id)
                        && execution
                            .as_ref()
                            .is_none_or(|execution| handle.thread_id == execution.thread_id
                                && handle.turn_id == execution.turn_id),
                    "the request belongs to another work turn"
                );
            } else if let Some(execution) = &execution {
                ensure!(
                    store
                        .work_native_execution(&control.network, work_id)?
                        .is_some_and(|reserved| reserved.thread_id == execution.thread_id
                            && reserved.turn_id == execution.turn_id),
                    "the request's work has no shared native turn permit"
                );
            }
            let work = store
                .work(&control.network, work_id)?
                .context("the request's work is missing")?;
            ensure!(
                work.target_node == request.node
                    && work.outcome.is_none()
                    && work.parent.as_ref() == parent.as_ref().map(|(id, _, _)| id)
                    && self
                        .providers()
                        .switches
                        .execution
                        .contains(&request.provider)
                    && self
                        .local_permission_epoch
                        .load(std::sync::atomic::Ordering::Acquire)
                        == request.permission_epoch,
                "the request's execution permission was revoked"
            );
            Self::validate_work_assignment_roots(&store, &control.network, &work.trigger)?;
            ensure!(
                (work.cwd.is_empty() || PathBuf::from(&work.cwd).canonicalize()? == actual_cwd)
                    && store.work_native_directory_matches_reservation(
                        &control.network,
                        &work,
                        actual_cwd
                            .to_str()
                            .context("native work directory is not UTF-8")?
                    )?,
                "the request's native directory differs from the reserved work"
            );
            if parent.is_none() {
                let owner = store
                    .conversation(&control.network, &record.conversation)?
                    .context("work conversation missing")?;
                ensure!(
                    owner.owner.as_deref() == Some(work.dispatcher.as_str())
                        && request.owner_epoch == Some(owner.owner_epoch),
                    "the request's work owner is no longer valid"
                );
            }
        }
        Ok(leases)
    }

    /// The connection that carried the held App Server requests is gone, and so are they.
    pub(crate) async fn drop_codex_requests(&self) -> Result<()> {
        let lost: Vec<_> = {
            let mut requests = lock(&self.triage.requests);
            let lost: Vec<_> = requests
                .iter()
                .filter(|(_, request)| matches!(request.reply, Reply::AppServer { .. }))
                .map(|(id, _)| id.clone())
                .collect();
            for id in &lost {
                requests.remove(id);
            }
            lost
        };
        let mut store = self.store.lock().await;
        for id in lost {
            tracing::info!(event = "request.lost", request = %id, "App Server request lost with its connection");
            store.finish_triage_request(&id, false)?;
        }
        Ok(())
    }

    /// Once the App Server finished observing this device's threads after a connect, any request
    /// it still wants has been asked again. An open question about a request that did not come
    /// back can never be answered, so this device, which asked it, closes it.
    pub(crate) async fn close_lost_questions(&self) -> Result<()> {
        if !self.has_membership().await? {
            return Ok(());
        }
        let network = self.network_id().await?;
        let held: HashSet<String> = lock(&self.triage.requests).keys().cloned().collect();
        let closed = self.store.lock().await.close_lost_questions(
            &self.identity,
            &network,
            &held,
            now_ms(),
        )?;
        for (scope, event) in closed {
            tracing::info!(
                event = "question.closed",
                conversation_id = scope,
                "a question about a lost App Server request was closed"
            );
            self.publish_changed(&network, &scope, Changed::Event(&event.hash));
        }
        Ok(())
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
                let ended = vibeface_bridge::turn::turn_snapshot(&params["turn"]);
                let status = ended
                    .status
                    .as_ref()
                    .map_or("completed", |status| status.as_str());
                let mut event = AgentEvent {
                    source: triage::Source::AppServer,
                    agent: Provider::Codex,
                    event: triage::Kind::TurnEnd,
                    status: Some(status.into()),
                    session_id: thread,
                    turn_id: ended.id.unwrap_or_default(),
                    cwd: entry.0.clone(),
                    last_message: std::mem::take(&mut entry.1),
                    detail: ended.error.map(|error| error.detail).unwrap_or_default(),
                    ..Default::default()
                };
                let node = self.clone();
                if let Err(error) = self.background_tasks.spawn(async move {
                    // Another CLI connection may have created the thread before this observer joined.
                    if event.cwd.is_empty()
                        && node.manager().is_some()
                        && let Ok(Ok(reply)) = tokio::time::timeout(
                            Duration::from_secs(5),
                            node.codex.client().request(
                                "thread/read",
                                json!({"threadId":event.session_id,"includeTurns":false}),
                            ),
                        )
                        .await
                        && reply["thread"]["id"].as_str() == Some(event.session_id.as_str())
                    {
                        if !triage::interactive_codex(&reply["thread"]["source"], None) {
                            let mut background = lock(&node.triage.background);
                            if background.len() > 1024 {
                                background.clear();
                            }
                            background.insert(event.session_id.clone());
                        }
                        if let Some(cwd) = reply["thread"]["cwd"]
                            .as_str()
                            .filter(|cwd| !cwd.is_empty() && cwd.len() <= 4096)
                        {
                            event.cwd = cwd.into();
                        }
                    }
                    if let Err(error) = node.ingest_agent_event(event).await {
                        tracing::debug!(%error, "App Server turn was not triaged");
                    }
                }) {
                    tracing::debug!(%error, "App Server triage skipped while the Node is shutting down");
                }
            }
            _ => {}
        }
    }

    pub(super) fn start_triage(self: &Arc<Self>) {
        let node = self.clone();
        if let Err(error) = self.background_tasks.spawn(async move {
            loop {
                tokio::select! {
                    _ = tokio::time::sleep(WAKE_EVERY) => {}
                    _ = node.triage.wake.notified() => {}
                }
                if let Err(error) = node.wake_agent().await {
                    tracing::warn!(event = "triage.wake_failed", %error, "could not hand triage events to the Agent");
                }
            }
        }) {
            tracing::debug!(%error, "triage event worker skipped while the Node is shutting down");
        }
        let node = self.clone();
        if let Err(error) = self.background_tasks.spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(5)).await;
                if let Err(error) = node.expire_requests().await {
                    tracing::warn!(event = "request.expire_failed", %error, "could not answer expired requests");
                }
                if let Err(error) = node.send_answered().await {
                    tracing::warn!(event = "webhook.answers_failed", %error, "could not tell the webhook about answers");
                }
            }
        }) {
            tracing::debug!(%error, "request-expiry worker skipped while the Node is shutting down");
        }
        let node = self.clone();
        if let Err(error) = self.background_tasks.spawn(async move {
            loop {
                if let Err(error) = node.deliver_webhooks().await {
                    tracing::warn!(event = "webhook.failed", %error, "webhook delivery failed");
                }
                let _ = tokio::time::timeout(Duration::from_secs(5), node.triage.outbox.notified())
                    .await;
            }
        }) {
            tracing::debug!(%error, "webhook worker skipped while the Node is shutting down");
        }
        let node = self.clone();
        if let Err(error) = self.background_tasks.spawn(async move {
            tokio::time::sleep(Duration::from_secs(5)).await;
            loop {
                if let Err(error) = node.scan_sources().await {
                    tracing::warn!(event = "sessions.scan_failed", %error, "session scan failed");
                }
                tokio::time::sleep(SCAN_EVERY).await;
            }
        }) {
            tracing::debug!(%error, "session scan worker skipped while the Node is shutting down");
        }
    }

    async fn pending_events(&self) -> Result<Vec<AgentEvent>> {
        self.store.lock().await.triage_events(None)
    }

    fn capture_answer_manager(&self) -> Result<Option<ManagerConfig>> {
        self.manager()
            .map(|settings| self.manager_config(settings, &self.providers()))
            .transpose()
    }

    /// One turn of this device's coordination Agent; it acts through the `vibeface` CLI.
    async fn coordination_turn(
        &self,
        attempt: Option<&str>,
        prompt: impl FnOnce(&str) -> Result<String>,
    ) -> Result<()> {
        let launch = self.manager_controls.lock().await;
        if self
            .pending_provider_launches
            .load(std::sync::atomic::Ordering::SeqCst)
        {
            return Err(manager::ManagerError::not_started(anyhow::anyhow!(
                "provider settings are pending"
            )));
        }
        let providers = self.providers();
        let settings = self
            .manager()
            .context("no coordination Agent is configured")
            .map_err(manager::ManagerError::not_started)?;
        let mut config = self
            .manager_config(settings, &providers)
            .map_err(manager::ManagerError::not_started)?;
        self.prepare_manager_login(&mut config)
            .await
            .map_err(manager::ManagerError::not_started)?;
        crate::scheduler::ensure_readiness(
            &self
                .provider_readiness(config.sdk.provider(), &config.model)
                .await,
        )?;
        drop(launch);
        self.run_coordination(config, attempt, prompt).await
    }

    async fn run_coordination(
        &self,
        mut config: ManagerConfig,
        attempt: Option<&str>,
        prompt: impl FnOnce(&str) -> Result<String>,
    ) -> Result<()> {
        let program = crate::sibling_cli().map_err(manager::ManagerError::not_started)?;
        let mut cli = format!("'{}'", program.to_string_lossy().replace('\'', r"'\''"));
        config.tool_cli = Some(program);
        config = config.for_cursor_instance(
            CursorInstance::Coordination,
            self.cursor_store_root(&CursorInstance::Coordination),
        );
        if let Some(attempt) = attempt {
            config
                .tool_env
                .insert("VIBEFACE_TRIAGE_ATTEMPT".into(), attempt.into());
            cli = format!("VIBEFACE_TRIAGE_ATTEMPT={attempt} {cli}");
        }
        let prompt = prompt(&cli).map_err(manager::ManagerError::not_started)?;
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
        manager::run_manager(&config, &prompt, None, record, cancel, |_| async { Ok(()) })
            .await
            .map(|_| ())
    }

    /// Hands waiting events, each with the bot that owns its session, to one turn of the
    /// coordination Agent. It answers through `vibeface triage` and `vibeface respond`.
    async fn wake_agent(&self) -> Result<()> {
        self.queue_live_answers().await?;
        let answering = self.store.lock().await.has_pending_answer_controls()?;
        if !answering && let Some(manager) = self.manager() {
            self.refresh_provider_login(manager.provider).await?;
        }
        if !answering && (!self.apply_provider_launches().await? || self.manager().is_none()) {
            return Ok(());
        }
        let Ok(_turn) = self.triage.turn.try_lock() else {
            return Ok(());
        };
        let network = self.network_id().await?;
        let admission = {
            let _mutations = self.triage.mutations.lock().await;
            self.store.lock().await.admit_triage(
                &network,
                &self.boot,
                BATCH,
                self.config.daily_decision_limit,
                now_ms(),
            )?
        };
        let attempt = match admission {
            TriageAdmission::Empty => return Ok(()),
            TriageAdmission::Paused(status) => {
                let pending = self.pending_events().await?;
                let mut requests = lock(&self.triage.requests);
                for event in &pending {
                    if let Some(id) = &event.request_id
                        && let Some(request) = requests.get_mut(id)
                    {
                        request.fallback_reason = Some("budget_exhausted");
                    }
                }
                let mut warning = lock(&self.triage.budget_warning);
                if warning.as_ref() != Some(&status) {
                    tracing::warn!(
                        event = "triage.budget_exhausted",
                        day = status.day,
                        used = status.used,
                        limit = status.limit,
                        retry_at_ms = status.retry_at_ms,
                        reason = "budget_exhausted",
                        "triage paused until its daily budget resets"
                    );
                    *warning = Some(status);
                }
                return Ok(());
            }
            TriageAdmission::Claimed(attempt) => attempt,
        };
        *lock(&self.triage.budget_warning) = None;
        *lock(&self.triage.active) = Some(attempt.id().into());
        let _active = ActiveTriage(&self.triage);
        if let Some(control) = attempt.answer() {
            let result = self.run_answer_control(attempt.id(), control).await;
            let _mutations = self.triage.mutations.lock().await;
            self.store.lock().await.finish_triage(attempt, &result)?;
            return result;
        }
        let fresh = attempt.events();
        for event in fresh {
            if let Some(id) = &event.request_id
                && let Some(request) = lock(&self.triage.requests).get_mut(id)
            {
                request.fallback_reason = None;
            }
        }
        let mut offered = Vec::new();
        let mut bots = Vec::new();
        let turn = async {
            let store = self.store.lock().await;
            let prepared = (|| -> Result<()> {
                let mut seen = HashSet::new();
                for event in fresh {
                    let owner = owner(&store, &network, event)?;
                    let bot = match &owner {
                        Some(scope) => store.bot_for_conversation(&network, scope)?,
                        None => None,
                    };
                    let name = match (&owner, &bot) {
                        (_, Some(bot)) => Some(bot.name.clone()),
                        (Some(scope), None) => {
                            store.conversation(&network, scope)?.map(|c| c.title)
                        }
                        (None, None) => None,
                    };
                    if let Some(bot) = bot
                        && seen.insert(bot.conversation_id.clone())
                    {
                        let notes = store.memories(&network, &bot.conversation_id)?;
                        bots.push(bot_context(&bot, &notes));
                    }
                    offered.push((
                        shared_event(event, &self.settings_snapshot().roots),
                        json!({"conversation_id":owner,"name":name}),
                    ));
                }
                Ok(())
            })()
            .map_err(manager::ManagerError::not_started);
            drop(store);
            prepared?;
            self.coordination_turn(Some(attempt.id()), |cli| {
                triage::wake_prompt(
                    &offered,
                    &bots,
                    cli,
                    AGENT_ANSWER_WITHIN.as_secs(),
                    HOOK_ANSWER_WITHIN.as_secs(),
                )
            })
            .await
        }
        .await;
        let _mutations = self.triage.mutations.lock().await;
        let turn = if turn.is_ok() {
            self.settle_offered(fresh).await
        } else {
            turn
        };
        self.store.lock().await.finish_triage(attempt, &turn)?;
        turn
    }

    async fn run_answer_control(&self, attempt: &str, control: &HeldAnswerControl) -> Result<()> {
        let request = self.validate_live_answer_control(attempt, control).await?;
        let (question, answer) = self.store.lock().await.answer_control_input(control)?;
        if let Some(result) = crate::store::triage::approval_answer_result(
            &question,
            &answer.text,
            &control.request.response,
        )? {
            let sent = self
                .triage_command(json!({"action":"respond","_attempt":attempt,
                "request_id":control.request.request_id,"result":result}))
                .await?;
            ensure!(
                sent["answered"] == true,
                "the original request's response is unconfirmed"
            );
            return Ok(());
        }
        let mut config = request
            .answer_manager
            .context("the original request has no captured coordination backend")?;
        ensure!(
            self.manager().is_some(),
            "coordination Agent permission was revoked"
        );
        drop(self.answer_permission_lease(control).await?);
        self.prepare_manager_login(&mut config)
            .await
            .map_err(manager::ManagerError::not_started)?;
        config.timeout_secs = config.timeout_secs.min(AGENT_ANSWER_WITHIN.as_secs());
        self.run_coordination(config, Some(attempt), |cli| {
            Ok(format!(
                "Deliver this user's stored answer to its original live request. This is a fresh coordination session, not a Task or Main turn. The user's signed message is the sole answer fact. Preserve its text. Do not approve permissions, change tool input, add permissions, create work, or send chat messages. Only use `{cli} requests` and `{cli} respond REQUEST_ID 'RESULT_JSON'` for the bound request. The Node validates the claim, transport generation, original schema, and result. If the schema cannot express the answer, leave the request unanswered. Your final text is discarded.\n{}",
                serde_json::to_string(&json!({"request_id":control.request.request_id,
                    "question":question,"answer":{"message_id":answer.message_id,"text":answer.text},
                    "response_schema":control.request.response}))?
            ))
        }).await?;
        ensure!(
            lock(&self.triage.answered)
                .get(&control.request.request_id)
                .is_some_and(|done| done["answered"] == true
                    && done["by"] == format!("answer:{}", control.answer_hash)),
            "the coordination turn did not confirm the stored answer's delivery"
        );
        Ok(())
    }

    async fn settle_offered(&self, offered: &[AgentEvent]) -> Result<()> {
        for event in offered {
            if let Some(request_id) = &event.request_id {
                let unattended = lock(&self.triage.requests)
                    .get(request_id)
                    .is_some_and(|request| request.question_id.is_none());
                if unattended {
                    self.answer_request(request_id, None, "fallback").await?;
                }
            }
        }
        Ok(())
    }

    pub(super) async fn triage_status(&self) -> Result<Value> {
        let network = self.store.lock().await.network()?.unwrap_or_default();
        let active = lock(&self.triage.active).clone();
        Ok(serde_json::to_value(
            self.store.lock().await.triage_status(
                &network,
                &self.boot,
                active.as_deref(),
                self.config.daily_decision_limit,
                now_ms(),
            )?,
        )?)
    }

    /// `/api/triage`, the Agents' side: `requests`, `respond`, `events`, `raise` and `dismiss`.
    pub(crate) async fn triage_command(&self, input: Value) -> Result<Value> {
        let network = self.network_id().await?;
        let action = super::field(&input, "action")?;
        let attempt = input
            .get("_attempt")
            .map(|value| value.as_str().context("invalid triage attempt context"))
            .transpose()?;
        let _mutations = if matches!(action, "respond" | "raise" | "dismiss") {
            Some(self.triage.mutations.lock().await)
        } else {
            None
        };
        let active = lock(&self.triage.active).clone();
        let context = attempt.map(|id| (self.boot.as_str(), id));
        let current = attempt.is_none() || attempt == active.as_deref();
        let answer_control = match attempt {
            Some(id) => self
                .store
                .lock()
                .await
                .answer_control_attempt(&self.boot, id)?,
            None => None,
        };
        if let Some(control) = answer_control {
            ensure!(
                matches!(action, "requests" | "respond"),
                "answer controls only read and respond to their original request"
            );
            ensure!(current, "answer control is no longer active");
            if let Some(node) = input.get("node") {
                ensure!(
                    node.as_str() == Some(control.request.node.as_str()),
                    "answer controls cannot target another Node"
                );
            }
            if action == "requests" {
                let request = self
                    .validate_live_answer_control(
                        attempt.context("answer control attempt missing")?,
                        &control,
                    )
                    .await?;
                return Ok(json!({"requests":[request]}));
            }
            let request_id = super::field(&input, "request_id")?;
            ensure!(
                request_id == control.request.request_id,
                "answer control cannot respond to another request"
            );
            let result = input.get("result").context("result is required")?;
            let (question, answer) = self.store.lock().await.answer_control_input(&control)?;
            validate_answer_result(&control.request.response, &question, &answer.text, result)?;
            let by = format!("answer:{}", control.answer_hash);
            if let Some(done) = lock(&self.triage.answered).get(request_id) {
                ensure!(
                    done["by"] == by,
                    "the request was answered by another actor"
                );
                return Ok(done.clone());
            }
            self.validate_live_answer_control(
                attempt.context("answer control attempt missing")?,
                &control,
            )
            .await?;
            let _permissions = self.config_save.lock().await;
            let denying = matches!(&control.request.response, AnswerResponse::Approval { deny, .. } if deny == result);
            let _leases = if denying {
                Vec::new()
            } else {
                self.answer_permission_lease(&control).await?
            };
            self.validate_live_answer_control(
                attempt.context("answer control attempt missing")?,
                &control,
            )
            .await?;
            return self
                .send_request_answer(
                    request_id,
                    Some(Ok(result.clone())),
                    &by,
                    Some(&control.request),
                )
                .await;
        }
        if action == "requests" {
            let mut requests: Vec<HeldRequest> =
                lock(&self.triage.requests).values().cloned().collect();
            if attempt.is_some() {
                ensure!(current, "triage attempt is no longer active");
                let events = self.store.lock().await.triage_events(context)?;
                requests.retain(|request| {
                    events
                        .iter()
                        .any(|event| event.request_id.as_deref() == Some(&request.request_id))
                });
            }
            return Ok(json!({"requests":requests}));
        }
        if action == "respond" {
            let request_id = super::field(&input, "request_id")?;
            let result = input.get("result").cloned().context("result is required")?;
            if let Some(done) = lock(&self.triage.answered).get(request_id) {
                return Ok(done.clone());
            }
            ensure!(current, "triage attempt is no longer active");
            if let Some(node) = input["node"].as_str()
                && node != self.identity.device_id()?
            {
                ensure!(
                    attempt.is_none(),
                    "a triage attempt only answers its local requests"
                );
                return self
                    .peer_request(
                        &network,
                        node,
                        json!({"type":"respond","request_id":request_id,"result":result}),
                    )
                    .await;
            }
            let human_question = attempt.is_none()
                && lock(&self.triage.requests)
                    .get(request_id)
                    .is_some_and(|request| request.question_id.is_some());
            ensure!(
                human_question
                    || self
                        .store
                        .lock()
                        .await
                        .triage_events(context)?
                        .iter()
                        .any(|event| event.request_id.as_deref() == Some(request_id)),
                "request is not available to this triage attempt"
            );
            return self
                .answer_request(request_id, Some(Ok(result)), "agent")
                .await;
        }
        if action == "events" {
            let since = input["since"].as_i64().unwrap_or(0);
            ensure!(current, "triage attempt is no longer active");
            let store = self.store.lock().await;
            let events: Vec<_> = store
                .triage_events(context)?
                .into_iter()
                .filter(|event| event.ts_ms >= since)
                .collect();
            let unknown = store
                .unknown_triage_events(active.as_deref().map(|id| (self.boot.as_str(), id)))?;
            return Ok(json!({"events":events,"unknown_events":unknown}));
        }
        let event_id = super::id(&input, "event_id")?;
        ensure!(
            !event_id.starts_with("answer:"),
            "answer controls are private to their claimed request"
        );
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
        ensure!(current, "triage attempt is no longer active");
        let event = self
            .store
            .lock()
            .await
            .triage_events(context)?
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
                        event.request_id.as_deref() == Some(request_id),
                        "request does not belong to this event"
                    );
                    ensure!(
                        lock(&self.triage.requests).contains_key(request_id),
                        "no request is waiting with this ID"
                    );
                }
                let held_request = request_id
                    .as_ref()
                    .and_then(|id| lock(&self.triage.requests).get(id).cloned());
                let approval = held_request
                    .as_ref()
                    .and_then(|request| match &request.response {
                        AnswerResponse::Approval { allow_once, .. } => Some(allow_once.is_some()),
                        _ => None,
                    });
                let choices = match approval {
                    Some(allow_once) => {
                        let mut choices = Vec::new();
                        if allow_once {
                            choices.push(Choice {
                                label: "允许本次".into(),
                                action: Some(crate::model::QuestionAction::AllowOnce),
                            });
                        }
                        choices.push(Choice {
                            label: "拒绝".into(),
                            action: Some(crate::model::QuestionAction::Deny),
                        });
                        choices
                    }
                    None => labels
                        .into_iter()
                        .map(|label| Choice {
                            label: crate::logging::scrub(&label),
                            action: None,
                        })
                        .collect(),
                };
                let question = EventBody::QuestionAsked {
                    question_id: triage::question_id(&event.node, &event.event_id),
                    target: None,
                    attention: AttentionRequest {
                        kind: match (approval, input["attention_kind"].as_str()) {
                            (Some(_), _) => AttentionKind::Approval,
                            (_, Some("browser_takeover")) => AttentionKind::BrowserTakeover,
                            (_, Some("approval")) => AttentionKind::Approval,
                            (_, Some("question")) => AttentionKind::Question,
                            (_, Some(_)) => bail!("unknown attention kind"),
                            (_, None) if event.hold == Some(RequestKind::Permission) => {
                                AttentionKind::Approval
                            }
                            (_, None) => AttentionKind::Question,
                        },
                        device_id: Some(event.node.clone()),
                    },
                    severity,
                    text: crate::logging::scrub(super::field(&input, "question")?),
                    choices,
                    about: subject(
                        &shared_event(&event, &self.settings_snapshot().roots),
                        request_id,
                    ),
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
    async fn raise(
        &self,
        network: &str,
        event: &AgentEvent,
        mut question: EventBody,
    ) -> Result<Value> {
        let (asked, hash, owner) = {
            let mut store = self.store.lock().await;
            let owner = match owner(&store, network, event)? {
                Some(owner) => owner,
                None => self
                    .claim_session(&mut store, network, event)?
                    .context("no bot owns this session")?,
            };
            if event.node == self.identity.device_id()?
                && let EventBody::QuestionAsked { target, about, .. } = &mut question
            {
                let native_turn = if matches!(&event.source, triage::Source::AppServer)
                    && event.request_id.is_some()
                {
                    event.request_id.as_deref().and_then(|id| {
                        lock(&self.triage.requests)
                            .get(id)
                            .filter(|request| {
                                request.request_id == id
                                    && about.request_id.as_deref() == Some(id)
                                    && request.provider == event.agent
                                    && request.session_id == event.session_id
                                    && !request.session_turn.is_empty()
                                    && matches!(&request.reply, Reply::AppServer { .. })
                            })
                            .map(|request| request.session_turn.clone())
                    })
                } else {
                    Some(event.turn_id.clone())
                };
                *target = if let Some(native_turn) = native_turn {
                    store.native_input_target(network, &owner, &event.session_id, &native_turn)?
                } else {
                    None
                };
            }
            let (asked, written) =
                store.ask(&self.identity, network, &owner, question, now_ms())?;
            (asked, written.map(|event| event.hash), owner)
        };
        let question_id = asked["question_id"].as_str().unwrap_or_default().to_owned();
        if let Some(request_id) = asked["about"]["request_id"].as_str()
            && let Some(request) = lock(&self.triage.requests).get_mut(request_id)
            && request.question_id.is_none()
        {
            if let Reply::AppServer { .. } = request.reply {
                request.answer_by_ms = None;
            }
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
            "answer":{"url":decision_url(&self.config)?, "device_id":self.identity.device_id()?,
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
                    event = "webhook.dead",
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
                .execute("DELETE FROM triage_seen WHERE ts_ms<? AND event_id NOT LIKE 'answer:%' AND NOT EXISTS(SELECT 1 FROM triage_queue q WHERE q.event_id=triage_seen.event_id)", [cutoff])?;
            store.connection.execute(
                "DELETE FROM triage_handled WHERE ts_ms<? AND event_id NOT LIKE 'answer:%'",
                [cutoff],
            )?;
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
            "decision_url":decision_url(&self.config)?, "device_id":self.identity.device_id()?,
            "decision_tokens":tokens, "outbox":outbox, "status":self.triage_status().await?,
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
        match &secret {
            None => {}
            Some(Value::Null) => {}
            Some(Value::String(secret)) => ensure!(
                (16..=512).contains(&secret.len()),
                "the webhook secret must be 16 to 512 characters"
            ),
            Some(_) => bail!("webhook_secret must be a string or null"),
        }
        self.write_providers(
            |target| {
                *target.triage = triage;
                Ok(())
            },
            None,
        )
        .await?;
        match secret {
            Some(Value::Null) => match std::fs::remove_file(self.data_dir.join(WEBHOOK_KEY)) {
                Err(error) if error.kind() != std::io::ErrorKind::NotFound => {
                    return Err(error.into());
                }
                _ => {}
            },
            Some(Value::String(secret)) => {
                config::private_write(&self.data_dir.join(WEBHOOK_KEY), secret.as_bytes())?;
            }
            _ => {}
        }
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
                AnswerAuthority::Signed,
            )
            .await?;
        Ok(
            json!({"status":answered.0,"question_id":answered.1.answers,"message":answered.1,
            "receipt":answered.2}),
        )
    }

    /// Persists the answer before returning. A held request queues private control work,
    /// while an ordinary question keeps its original scheduling target.
    async fn post_answer(
        &self,
        network: &str,
        question: &str,
        text: &str,
        attestation: Option<crate::model::Attestation>,
        authority: AnswerAuthority,
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
        let node = self.identity.device_id()?;
        let question_body = self
            .store
            .lock()
            .await
            .question(network, question)?
            .context("no such question")?;
        let binding = lock(&self.triage.requests)
            .values()
            .find(|request| request.question_id.as_deref() == Some(question))
            .map(|request| request.answer_binding(&node));
        ensure!(
            !question_body["about"]["request_id"].is_string()
                || question_body["about"]["node"] != node
                || binding.is_some()
                || !question_body["answer"].is_null()
                || !question_body["closed"].is_null(),
            "the original request is no longer waiting on this Node"
        );
        let answered = self.store.lock().await.accept_answer_with_control(
            &self.identity,
            network,
            question,
            text,
            attestation,
            now_ms(),
            binding.as_ref().map(|request| (request, authority)),
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
        self.triage.wake.notify_one();
        Ok(("answered", answered.message, answered.receipt))
    }

    /// `POST /api/decision`: a Node handles its own sessions and forwards the rest; the target
    /// alone checks the token.
    pub(crate) async fn decision_http(self: &Arc<Self>, token: &str, body: Value) -> Result<Value> {
        let input: DecisionInput =
            serde_json::from_value(body.clone()).context("invalid decision")?;
        net::endpoint_id(&input.device_id)?;
        let forwarded = json!({"token":token,"decision":body});
        if input.device_id == self.identity.device_id()? {
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
            .post_answer(
                &network,
                &input.question_id,
                &text,
                None,
                AnswerAuthority::DecisionToken,
            )
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
pub(super) mod tests {
    use super::*;
    use crate::{
        config::Config,
        crypto::LocalIdentity,
        identity::{Genesis, Member, NetworkState},
        model::{ResolvedModel, Role},
    };
    use std::{collections::BTreeMap, sync::atomic::Ordering};
    use vibeface_bridge::failure::FailureKind;

    #[test]
    fn decisions_reach_a_node_without_public_https_through_the_public_node() -> Result<()> {
        let mut config = Config::local("127.0.0.1:4792".parse().unwrap(), vec![], false);
        assert_eq!(decision_url(&config)?, "http://127.0.0.1:4792/api/decision");
        config.network.bootstrap = net::bootstrap::Bootstrap::Public;
        assert_eq!(
            decision_url(&config)?,
            "https://node.vibeface.dev/api/decision"
        );
        config.public_url = "https://node.example/".into();
        assert_eq!(decision_url(&config)?, "https://node.example/api/decision");
        Ok(())
    }

    async fn bare_node() -> Result<(Arc<Runtime>, PathBuf)> {
        node_at(LocalIdentity::generate(), Default::default()).await
    }

    async fn node_at(
        identity: LocalIdentity,
        network: crate::net::NetworkConfig,
    ) -> Result<(Arc<Runtime>, PathBuf)> {
        let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../.artifacts")
            .join(format!("triage-{}", uuid::Uuid::new_v4()));
        config::private_dir(&dir)?;
        let mut configuration = Config::local("127.0.0.1:0".parse()?, vec![], false);
        // A unix socket path must stay short; nothing listens unless a test binds it.
        #[cfg(unix)]
        let socket = PathBuf::from(format!(
            "/tmp/vf-{}.sock",
            &uuid::Uuid::new_v4().simple().to_string()[..12]
        ));
        #[cfg(windows)]
        let socket =
            vibeface_bridge::platform::ipc::Endpoint::in_private_dir(&dir, "fixture.sock")?
                .path()
                .to_owned();
        configuration.providers.codex_app_server.mode = crate::providers::AppServerMode::System;
        configuration.providers.codex_app_server.socket = Some(socket);
        configuration.providers.execution = [crate::providers::Provider::Codex].into();
        configuration.network = network;
        configuration.save(&dir)?;
        config::private_write(
            &dir.join("data-epoch.json"),
            &crate::config::data_epoch_marker(),
        )?;
        config::private_write(&dir.join("identity.json"), &serde_json::to_vec(&identity)?)?;
        Ok((Arc::new(Runtime::open(&dir).await?), dir))
    }

    pub(crate) async fn node() -> Result<(Arc<Runtime>, LocalIdentity, PathBuf)> {
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
            provider: crate::providers::Provider::Codex,
            workspace: std::env::temp_dir(),
            model: "fixture".into(),
            reasoning_effort: None,
            backup: Default::default(),
        });
    }

    pub(crate) async fn bot(
        node: &Runtime,
        admin: &LocalIdentity,
        scope: &str,
        name: &str,
        threads: &[&str],
    ) -> Result<()> {
        instructed_bot(node, admin, scope, name, "", threads).await
    }

    async fn instructed_bot(
        node: &Runtime,
        admin: &LocalIdentity,
        scope: &str,
        name: &str,
        instructions: &str,
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
                instructions: instructions.into(),
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

    async fn main_input_fact(
        node: &Runtime,
        admin: &LocalIdentity,
        scope: &str,
        id: &str,
    ) -> Result<String> {
        let network = node.network_id().await?;
        let mut store = node.store.lock().await;
        let bot = store
            .bot_for_conversation(&network, scope)?
            .context("input Bot")?;
        let target = crate::workspace::InputTarget::BotMain { bot_id: bot.bot_id };
        let trigger = crate::scheduling::message_trigger(id);
        if store.message(&network, scope, id)?.is_none() {
            let posted = admin.attest(
                &network,
                scope,
                EventBody::InputPosted {
                    message_id: id.into(),
                    target: target.clone(),
                    intent: crate::workspace::InputIntent::Continue,
                    sequence: store.messages(&network, scope)?.len() as u64 + 1,
                    expected_revision: None,
                    text: id.into(),
                    attachments: vec![],
                    report_results: Some(true),
                    attestation: None,
                },
            )?;
            store.append_event(&node.identity, &network, scope, posted, vec![], now_ms())?;
        }
        ensure!(
            store.trigger_input_target(&network, scope, &trigger)? == Some(target),
            "fixture input has no verified Main target"
        );
        Ok(trigger)
    }

    async fn complete_tracked_work(
        node: &Arc<Runtime>,
        network: &str,
        work_id: &str,
    ) -> Result<()> {
        let turn_id = format!("turn-{work_id}");
        node.signal_work(
            network,
            work_id,
            crate::work::Signal::TurnStarted {
                turn_id: turn_id.clone(),
                epoch: node.codex.epoch(),
            },
        )
        .await?;
        let completed = node
            .signal_work(
                network,
                work_id,
                crate::work::Signal::TurnCompleted {
                    turn_id,
                    status: vibeface_bridge::turn::TurnStatus::Completed,
                    text: "done".into(),
                    error: None,
                },
            )
            .await?;
        assert_eq!(
            completed.state,
            crate::work::WorkState::Finished {
                outcome: crate::model::WorkOutcome::Completed,
            }
        );
        Ok(())
    }

    async fn external_work_thread_fact(
        node: &Arc<Runtime>,
        admin: &LocalIdentity,
        thread: &str,
    ) -> Result<()> {
        let trigger = main_input_fact(node, admin, "c", &format!("triage-origin-{thread}")).await?;
        let network = node.network_id().await?;
        let local = node.identity.device_id()?;
        let work_id = format!("work-external-{thread}");
        node.store.lock().await.accept_work_fixture(
            &node.identity,
            &network,
            crate::work::WorkSpec {
                work_id: work_id.clone(),
                conversation: "c".into(),
                trigger,
                target: local.clone(),
                dispatcher: local,
                call_ref: work_id.clone(),
                kind: crate::model::WorkKind::Agent(Provider::Codex),
                model: None,
                resolved: ResolvedModel {
                    provider: Provider::Codex,
                    model: None,
                },
                cwd: node.data_dir.canonicalize()?.display().to_string(),
                title: "external work".into(),
                prompt: "external work".into(),
                continues: None,
                parent: None,
                thread: Some(thread.into()),
            },
            now_ms(),
        )?;
        node.signal_work(
            &network,
            &work_id,
            crate::work::Signal::TurnStarted {
                turn_id: "t1".into(),
                epoch: node.codex.epoch(),
            },
        )
        .await?;
        let store = node.store.lock().await;
        ensure!(
            !own_turn(&store, &turn(thread, "t1", "/w"))?,
            "an external work is not a coordination turn"
        );
        ensure!(
            store.native_input_target(&network, "c", thread, "t1")?
                == Some(crate::workspace::InputTarget::Work { work_id }),
            "external session has no verified Work target"
        );
        Ok(())
    }

    async fn unanswered(node: &Runtime) -> Result<Vec<Value>> {
        let network = node.network_id().await?;
        node.store.lock().await.unanswered_questions(&network)
    }

    async fn answer(node: &Runtime, question: &str, text: &str) -> Result<&'static str> {
        let network = node.network_id().await?;
        Ok(node
            .post_answer(
                &network,
                question,
                text,
                None,
                AnswerAuthority::DecisionToken,
            )
            .await?
            .0)
    }

    async fn signed_answer_fact(
        node: &Runtime,
        admin: &LocalIdentity,
        question: &str,
        text: &str,
    ) -> Result<Value> {
        let network = node.network_id().await?;
        let scope = node
            .store
            .lock()
            .await
            .question(&network, question)?
            .context("question")?["conversation_id"]
            .as_str()
            .context("question scope")?
            .to_owned();
        let signed = admin.attest(
            &network,
            &scope,
            EventBody::MessagePosted {
                message_id: crate::store::questions::answer_id(question),
                role: crate::model::Role::User,
                text: text.into(),
                attachments: vec![],
                report_results: Some(true),
                in_reply_to: None,
                status: None,
                answers: Some(question.into()),
                attestation: None,
                failure: None,
            },
        )?;
        let result = node
            .post_answer(
                &network,
                question,
                text,
                signed.attestation().cloned(),
                AnswerAuthority::Signed,
            )
            .await?;
        Ok(json!({"status":result.0,"message":result.1,"receipt":result.2}))
    }

    type Server = tokio_tungstenite::WebSocketStream<vibeface_bridge::platform::ipc::Stream>;

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

    fn handed(id: i64, method: &str, params: Value) -> ServerRequest {
        ServerRequest {
            id: vibeface_protocol::RpcId::Number(id),
            method: method.into(),
            thread_id: params["threadId"].as_str().map(str::to_owned),
            params,
        }
    }

    async fn app_server(node: &Runtime) -> Result<(Server, ServerRequest)> {
        use futures_util::SinkExt as _;
        let mut listener = vibeface_bridge::platform::ipc::Listener::bind(
            &vibeface_bridge::platform::ipc::Endpoint::from_path(&named_socket(node))?,
        )
        .await?;
        let stream = tokio::time::timeout(Duration::from_secs(5), listener.accept()).await??;
        let mut server = tokio_tungstenite::accept_async(stream).await?;
        let initialize = read(&mut server).await;
        let ready = json!({"id":initialize["id"],"result":{"userAgent":"codex/fixture"}});
        server.send(ready.to_string().into()).await?;
        assert_eq!(read(&mut server).await["method"], "initialized");
        let params = json!({"threadId":"t-1","turnId":"u-1","itemId":"fixture-approval","command":"rm -rf build","reason":"clean the build"});
        let request =
            json!({"id":41,"method":"item/commandExecution/requestApproval","params":params});
        server.send(request.to_string().into()).await?;
        while node.codex.client().snapshot().await["pending_requests"] == json!([]) {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        Ok((
            server,
            handed(41, "item/commandExecution/requestApproval", params),
        ))
    }

    async fn answered_once(server: &mut Server) -> Value {
        let answer = read(server).await;
        assert_eq!(answer["id"], 41);
        let again = tokio::time::timeout(Duration::from_millis(300), read(server)).await;
        assert!(again.is_err(), "the request is answered once");
        answer
    }

    async fn cleanup(node: Arc<Runtime>, dir: PathBuf) -> Result<()> {
        node.shutdown().await;
        #[cfg(unix)]
        match std::fs::remove_file(named_socket(&node)) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        drop(node);
        std::fs::remove_dir_all(dir)?;
        Ok(())
    }

    async fn answering_app_server(
        node: &Runtime,
    ) -> Result<tokio::sync::mpsc::UnboundedReceiver<Value>> {
        scripted_app_server(node, |_| json!({"result":{}})).await
    }

    /// An App Server fixture: `answer` gives each request's reply, `{"result":…}` or `{"error":…}`.
    pub(in crate::runtime) async fn scripted_app_server(
        node: &Runtime,
        mut answer: impl FnMut(&Value) -> Value + Send + 'static,
    ) -> Result<tokio::sync::mpsc::UnboundedReceiver<Value>> {
        use futures_util::{SinkExt as _, StreamExt as _};
        let mut listener = vibeface_bridge::platform::ipc::Listener::bind(
            &vibeface_bridge::platform::ipc::Endpoint::from_path(&named_socket(node))?,
        )
        .await?;
        let (sent, received) = tokio::sync::mpsc::unbounded_channel();
        tokio::spawn(async move {
            let Ok(stream) = listener.accept().await else {
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
                    let mut reply = match request["method"].as_str() {
                        Some("initialize") => json!({"result":{"userAgent":"codex/fixture"}}),
                        _ => answer(&request),
                    };
                    reply["id"] = request["id"].clone();
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
        node.shutdown().await;
        drop(node);
        std::fs::remove_dir_all(dir)?;
        result
    }

    #[tokio::test]
    async fn a_work_stopped_while_starting_is_interrupted_once_bound() -> Result<()> {
        let (node, admin, dir) = node().await?;
        let result = async {
            with_agent(&node);
            bot(&node, &admin, "c", "前端", &[]).await?;
            let mut requests = answering_app_server(&node).await?;
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
                kind: crate::model::WorkKind::Agent(crate::providers::Provider::Codex),
                model: None,
                resolved: crate::model::ResolvedModel {
                    provider: crate::providers::Provider::Codex,
                    model: None,
                },
                cwd: "/tmp".into(),
                title: "t".into(),
                prompt: "p".into(),
                continues: None,
                parent: None,
                thread: Some("t-9".into()),
            };
            node.store.lock().await.accept_work_fixture(
                &node.identity,
                &network,
                spec,
                now_ms(),
            )?;
            let stopped = node.interrupt_work(&network, "w-stop").await?;
            assert_eq!(stopped["waiting_for_start"], true);
            let waiting = node.store.lock().await.stop_requests()?;
            assert!(waiting.iter().any(|request| request.work_id == "w-stop"));

            let epoch = node.codex.epoch();
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
            let held = node.store.lock().await.stop_requests()?;
            assert!(
                held.iter()
                    .any(|request| request.work_id == "w-stop" && !request.timed_out),
                "the stop is held until the turn ends"
            );
            node.deliver_work_requests(&network).await?;
            node.reconcile_works(&network).await?;
            while let Ok(request) = requests.try_recv() {
                assert_ne!(request["method"], "turn/interrupt", "the stop is sent once");
            }
            Ok::<_, anyhow::Error>(())
        }
        .await;
        cleanup(node, dir).await?;
        result
    }

    /// A Codex work on `node` whose turn `u-1` runs in thread `t-9`.
    async fn running_work(node: &Arc<Runtime>, admin: &LocalIdentity, work_id: &str) -> Result<()> {
        let network = node.network_id().await?;
        let local = node.identity.device_id()?;
        let trigger = main_input_fact(node, admin, "c", "m1").await?;
        let spec = crate::work::WorkSpec {
            work_id: work_id.into(),
            conversation: "c".into(),
            trigger,
            target: local.clone(),
            dispatcher: local,
            call_ref: work_id.into(),
            kind: crate::model::WorkKind::Agent(crate::providers::Provider::Codex),
            model: None,
            resolved: crate::model::ResolvedModel {
                provider: crate::providers::Provider::Codex,
                model: None,
            },
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
            .accept_work_fixture(&node.identity, &network, spec, now_ms())?;
        let epoch = node.codex.epoch();
        let turn = crate::work::Signal::TurnStarted {
            turn_id: "u-1".into(),
            epoch,
        };
        let running = node.signal_work(&network, work_id, turn).await?;
        assert!(matches!(
            running.state,
            crate::work::WorkState::Running { .. }
        ));
        Ok(())
    }

    async fn connected(node: &Runtime) {
        while node.codex.status().await["connection"]["state"] != "connected" {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    /// The Node's turn reads and interrupts, in order. The bridge's thread observation polls a
    /// live thread every few seconds with `includeTurns: false`; it is not the Node's.
    fn methods(requests: &mut tokio::sync::mpsc::UnboundedReceiver<Value>) -> Vec<String> {
        std::iter::from_fn(|| requests.try_recv().ok())
            .filter(|request| match request["method"].as_str() {
                Some("turn/interrupt") => true,
                Some("thread/read") => request["params"]["includeTurns"] == true,
                _ => false,
            })
            .filter_map(|request| request["method"].as_str().map(str::to_owned))
            .collect()
    }

    async fn state(node: &Runtime, work_id: &str) -> Result<crate::work::WorkState> {
        Ok(node
            .store
            .lock()
            .await
            .work_record(work_id)?
            .context("work")?
            .state)
    }

    async fn held_stop(node: &Runtime, work_id: &str) -> Result<Option<crate::work::StopRequest>> {
        Ok(node
            .store
            .lock()
            .await
            .stop_requests()?
            .into_iter()
            .find(|request| request.work_id == work_id))
    }

    /// A `turn/start` stuck past the send window is not written off as never run: the work is
    /// unverified, and the turn that shows up late is followed to its result.
    #[tokio::test]
    async fn a_send_stuck_past_its_window_is_unverified_then_followed() -> Result<()> {
        use crate::model::{WorkOutcome, WorkStatus};
        use std::sync::atomic::{AtomicBool, Ordering};
        let (node, admin, dir) = node().await?;
        let result = async {
            bot(&node, &admin, "c", "前端", &[]).await?;
            let started = Arc::new(AtomicBool::new(false));
            let seen = started.clone();
            let _requests =
                scripted_app_server(&node, move |request| match request["method"].as_str() {
                    Some("thread/read") if seen.load(Ordering::SeqCst) => {
                        json!({"result":{"thread":{"turns":[{"id":"u-1","status":"inProgress",
                            "items":[{"type":"userMessage","clientId":"w-late"}]}]}}})
                    }
                    Some("thread/read") => json!({"result":{"thread":{"turns":[]}}}),
                    _ => json!({"result":{}}),
                })
                .await?;
            connected(&node).await;
            let network = node.network_id().await?;
            let local = node.identity.device_id()?;
            let trigger = main_input_fact(&node, &admin, "c", "m1").await?;
            let spec = crate::work::WorkSpec {
                work_id: "w-late".into(),
                conversation: "c".into(),
                trigger,
                target: local.clone(),
                dispatcher: local,
                call_ref: "late".into(),
                kind: crate::model::WorkKind::Agent(crate::providers::Provider::Codex),
                model: None,
                resolved: crate::model::ResolvedModel {
                    provider: crate::providers::Provider::Codex,
                    model: None,
                },
                cwd: "/tmp".into(),
                title: "t".into(),
                prompt: "p".into(),
                continues: None,
                parent: None,
                thread: None,
            };
            let window = crate::work::SEND_WINDOW_MS + crate::work::RECONCILE_GRACE_MS;
            node.store.lock().await.accept_work_fixture(
                &node.identity,
                &network,
                spec,
                now_ms() - window,
            )?;
            node.store.lock().await.apply_work_signal(
                &node.identity,
                &network,
                "w-late",
                crate::work::Signal::ThreadCreated("t-late".into()),
                now_ms(),
            )?;
            node.reconcile_works(&network).await?;
            let outcome = async || -> Result<(Option<WorkOutcome>, WorkStatus)> {
                let work = node
                    .store
                    .lock()
                    .await
                    .works(&network, "c")?
                    .into_iter()
                    .find(|work| work.work_id == "w-late")
                    .context("work")?;
                Ok((work.outcome, work.state))
            };
            assert_eq!(
                outcome().await?,
                (Some(WorkOutcome::Unverified), WorkStatus::Unverified),
                "an unseen send is not recorded as refused"
            );
            assert!(matches!(
                state(&node, "w-late").await?,
                crate::work::WorkState::Unverified { handle: None, thread: Some(ref thread), .. }
                    if thread == "t-late"
            ));
            assert!(
                node.bind_work(
                    &network,
                    "c",
                    "w-late",
                    vibeface_bridge::codex::StartReport::Thread {
                        thread_id: "t-late".into()
                    }
                )
                .await
                .is_err(),
                "past its window the work takes no new thread"
            );

            started.store(true, Ordering::SeqCst);
            let late = vibeface_bridge::codex::StartReport::Turn {
                turn_id: "u-1".into(),
            };
            node.bind_work(&network, "c", "w-late", late).await?;
            assert!(matches!(
                state(&node, "w-late").await?,
                crate::work::WorkState::Running { ref handle } if handle.turn_id == "u-1"
            ));
            let done = crate::work::Signal::TurnCompleted {
                turn_id: "u-1".into(),
                status: vibeface_bridge::turn::TurnStatus::Completed,
                text: "done".into(),
                error: None,
            };
            node.signal_work(&network, "w-late", done).await?;
            assert_eq!(
                outcome().await?,
                (Some(WorkOutcome::Completed), WorkStatus::Finished),
                "the late turn's real result replaces the unknown one"
            );
            assert!(
                node.store
                    .lock()
                    .await
                    .scheduler_job("c", &crate::scheduling::work_trigger("w-late"))?
                    .is_some(),
                "and reports back"
            );
            Ok::<_, anyhow::Error>(())
        }
        .await;
        cleanup(node, dir).await?;
        result
    }

    #[tokio::test]
    async fn reconciliation_reads_a_running_turn_whose_end_was_missed() -> Result<()> {
        let (node, admin, dir) = node().await?;
        let result = async {
            with_agent(&node);
            bot(&node, &admin, "c", "前端", &[]).await?;
            let mut requests =
                scripted_app_server(&node, move |request| match request["method"].as_str() {
                    Some("thread/read") => json!({"result":{"thread":{"turns":[
                        {"id":"u-1","status":"completed","items":[]}]}}}),
                    _ => json!({"result":{}}),
                })
                .await?;
            connected(&node).await;
            let network = node.network_id().await?;
            running_work(&node, &admin, "w-missed").await?;
            node.reconcile_works(&network).await?;
            assert_eq!(methods(&mut requests), [] as [&str; 0], "not due yet");

            node.live_probe_at
                .store(now_ms() - 60_000, std::sync::atomic::Ordering::Relaxed);
            node.reconcile_works(&network).await?;
            // The background reconciler may read too; reads are harmless, a resend is not.
            let sent = methods(&mut requests);
            assert!(
                !sent.is_empty() && sent.iter().all(|method| method == "thread/read"),
                "read, never resent: {sent:?}"
            );
            assert!(matches!(
                state(&node, "w-missed").await?,
                crate::work::WorkState::Finished {
                    outcome: crate::model::WorkOutcome::Completed
                }
            ));
            node.reconcile_works(&network).await?;
            assert_eq!(methods(&mut requests), [] as [&str; 0]);
            Ok::<_, anyhow::Error>(())
        }
        .await;
        cleanup(node, dir).await?;
        result
    }

    #[tokio::test]
    async fn a_held_stop_outlives_a_failed_interrupt_and_ends_with_its_work() -> Result<()> {
        use std::sync::atomic::{AtomicBool, Ordering};
        let (node, admin, dir) = node().await?;
        let result = async {
            with_agent(&node);
            bot(&node, &admin, "c", "前端", &[]).await?;
            let refuse = Arc::new(AtomicBool::new(true));
            let refusing = refuse.clone();
            let mut requests =
                scripted_app_server(&node, move |request| match request["method"].as_str() {
                    Some("turn/interrupt") if refusing.load(Ordering::SeqCst) => {
                        json!({"error":{"code":-32603,"message":"busy"}})
                    }
                    Some("thread/read") => json!({"result":{"thread":{"turns":[
                        {"id":"u-1","status":"inProgress","items":[]}]}}}),
                    _ => json!({"result":{}}),
                })
                .await?;
            connected(&node).await;
            let network = node.network_id().await?;
            running_work(&node, &admin, "w-held").await?;

            let stopped = node.interrupt_work(&network, "w-held").await?;
            assert_eq!(stopped["retrying"], true);
            assert!(matches!(
                state(&node, "w-held").await?,
                crate::work::WorkState::Running { .. }
            ));
            assert!(
                held_stop(&node, "w-held").await?.is_some(),
                "a failed interrupt keeps the stop"
            );
            assert_eq!(methods(&mut requests), ["turn/interrupt"]);
            node.reconcile_works(&network).await?;
            assert_eq!(methods(&mut requests), [] as [&str; 0], "retries back off");

            // Only the pacing is in memory, as after a restart; the stop itself is stored.
            refuse.store(false, Ordering::SeqCst);
            node.stop_retries.lock().unwrap().clear();
            node.reconcile_works(&network).await?;
            assert_eq!(
                methods(&mut requests),
                ["thread/read", "turn/interrupt"],
                "the turn is read before the interrupt is sent again"
            );
            assert!(matches!(
                state(&node, "w-held").await?,
                crate::work::WorkState::Interrupting { .. }
            ));
            let held = held_stop(&node, "w-held").await?.context("held")?;
            assert!(!held.timed_out && held.deadline_ms > now_ms());
            node.reconcile_works(&network).await?;
            assert_eq!(
                methods(&mut requests),
                [] as [&str; 0],
                "before the deadline"
            );

            // The stored deadline passes with the turn still running: it is asked again and
            // shows as still trying.
            let mut late = held;
            late.deadline_ms = now_ms() - 1;
            node.store.lock().await.save_stop_request(&late)?;
            node.reconcile_works(&network).await?;
            assert_eq!(methods(&mut requests), ["thread/read", "turn/interrupt"]);
            assert!(held_stop(&node, "w-held").await?.context("held")?.timed_out);
            assert!(matches!(
                state(&node, "w-held").await?,
                crate::work::WorkState::Interrupting { .. }
            ));

            let ended = crate::work::Signal::TurnCompleted {
                turn_id: "u-1".into(),
                status: vibeface_bridge::turn::TurnStatus::Interrupted,
                text: String::new(),
                error: None,
            };
            node.signal_work(&network, "w-held", ended).await?;
            assert!(held_stop(&node, "w-held").await?.is_none());
            node.stop_retries.lock().unwrap().clear();
            node.reconcile_works(&network).await?;
            let again = node.interrupt_work(&network, "w-held").await?;
            assert_eq!(again["retrying"], false);
            assert_eq!(
                methods(&mut requests),
                [] as [&str; 0],
                "an ended turn is never interrupted again"
            );
            Ok::<_, anyhow::Error>(())
        }
        .await;
        cleanup(node, dir).await?;
        result
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_stop_past_its_deadline_reaches_a_device_that_comes_back() -> Result<()> {
        let (ia, ib) = (LocalIdentity::generate(), LocalIdentity::generate());
        let target = ib.device_id()?;
        let membership = ia.sign_network_fixture(1, vec![ia.device_id()?, target.clone()])?;
        let admin = ia.fixture_admin();
        let networked = || crate::net::NetworkConfig {
            bind: Some("127.0.0.1:0".parse().expect("address")),
            ..Default::default()
        };
        let (a, dir_a) = node_at(ia, networked()).await?;
        let (b, dir_b) = node_at(ib, networked()).await?;
        let result = async {
            for node in [&a, &b] {
                node.install_membership(membership.clone()).await?;
            }
            let network = b.network_id().await?;
            bot(&b, &admin, "c", "前端", &[]).await?;
            let mut requests = answering_app_server(&b).await?;
            connected(&b).await;
            running_work(&b, &admin, "w-far").await?;

            // B is offline and the stop is past its deadline.
            a.start_network().await?;
            let at = now_ms();
            let stop = crate::work::StopRequest {
                work_id: "w-far".into(),
                conversation: "c".into(),
                target: target.clone(),
                requested_at_ms: at - 60_000,
                deadline_ms: at - 30_000,
                timed_out: true,
            };
            a.store.lock().await.save_stop_request(&stop)?;
            a.deliver_work_requests(&network).await?;
            assert!(
                held_stop(&a, "w-far")
                    .await?
                    .is_some_and(|held| held.timed_out),
                "an unconfirmed stop is never given up"
            );

            b.start_network().await?;
            a.net()?.learn(&b.net()?.addr()).await;
            // Retry by hand: the backoff from the offline attempts must not outlast the timeout.
            let interrupt = tokio::time::timeout(Duration::from_secs(30), async {
                loop {
                    a.stop_retries.lock().unwrap().clear();
                    a.deliver_work_requests(&network).await?;
                    let Ok(request) =
                        tokio::time::timeout(Duration::from_millis(500), requests.recv()).await
                    else {
                        continue;
                    };
                    let request = request.context("the App Server closed")?;
                    if request["method"] == "turn/interrupt" {
                        return Ok::<_, anyhow::Error>(request);
                    }
                }
            })
            .await??;
            assert_eq!(
                interrupt["params"],
                json!({"threadId":"t-9","turnId":"u-1"})
            );
            while held_stop(&a, "w-far").await?.is_some() {
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            assert!(matches!(
                state(&b, "w-far").await?,
                crate::work::WorkState::Interrupting { .. }
            ));
            assert!(
                held_stop(&b, "w-far").await?.is_some(),
                "B holds the stop it took"
            );

            let ended = crate::work::Signal::TurnCompleted {
                turn_id: "u-1".into(),
                status: vibeface_bridge::turn::TurnStatus::Interrupted,
                text: String::new(),
                error: None,
            };
            b.signal_work(&network, "w-far", ended).await?;
            a.sync_peer(&network, &target).await?;
            assert!(
                a.store
                    .lock()
                    .await
                    .works(&network, "c")?
                    .iter()
                    .any(|work| work.work_id == "w-far" && work.outcome.is_some())
            );
            a.store.lock().await.save_stop_request(&stop)?;
            a.stop_retries.lock().unwrap().clear();
            tokio::time::timeout(Duration::from_secs(5), async {
                loop {
                    a.deliver_work_requests(&network).await?;
                    if held_stop(&a, "w-far").await?.is_none() {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
                Ok::<_, anyhow::Error>(())
            })
            .await
            .context("terminal work drains the asynchronous stop worker")??;
            assert!(held_stop(&a, "w-far").await?.is_none());
            b.stop_retries.lock().unwrap().clear();
            b.reconcile_works(&network).await?;
            assert_eq!(
                methods(&mut requests),
                [] as [&str; 0],
                "an ended work gets no stop"
            );
            Ok::<_, anyhow::Error>(())
        }
        .await;
        for node in [&a, &b] {
            node.stop_network().await;
        }
        cleanup(a, dir_a).await?;
        cleanup(b, dir_b).await?;
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
            let deadline = lock(&node.triage.requests)[&request_id].answer_by_ms;
            node.store
                .lock()
                .await
                .finish_triage_request(&request_id, false)?;
            node.on_codex_request(&handed).await;
            assert_eq!(
                lock(&node.triage.requests)[&request_id].answer_by_ms,
                deadline,
                "a repeated live App Server request keeps its original deadline"
            );
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

    fn asks_permission(session: &str) -> AgentEvent {
        AgentEvent {
            agent: Provider::Claude,
            event: triage::Kind::NeedsInput,
            hold: Some(RequestKind::Permission),
            ..turn(session, "t1", "/w")
        }
    }

    #[tokio::test]
    async fn a_hook_waits_for_the_answer_to_its_request() -> Result<()> {
        let (node, admin, dir) = node().await?;
        let result = async {
            with_agent(&node);
            bot(&node, &admin, "c", "前端", &[]).await?;
            let held = node.ingest_agent_event(asks_permission("s1")).await?;
            let request_id = held["request_id"]
                .as_str()
                .context("a held request")?
                .to_owned();
            assert!(
                node.ingest_agent_event(asks_permission("s1")).await?["request_id"].is_null(),
                "a request is held once"
            );
            let listed =
                node.triage_command(json!({"action":"requests"})).await?["requests"][0].clone();
            assert_eq!(
                (listed["provider"].clone(), listed["kind"].clone()),
                (json!("claude"), json!("permission"))
            );
            assert_eq!(
                listed["answer_by_ms"].as_i64().context("a deadline")?
                    - listed["received_ms"].as_i64().context("received")?,
                HOOK_ANSWER_WITHIN.as_millis() as i64,
                "a hook's request is held only as long as its session can wait"
            );
            let waiting = tokio::spawn({
                let node = node.clone();
                let request_id = request_id.clone();
                async move { node.wait_request(&request_id).await }
            });
            tokio::task::yield_now().await;
            // What a hook's wait sees once the request is gone from the held ones.
            let seen = std::thread::spawn({
                let node = node.clone();
                let request_id = request_id.clone();
                move || loop {
                    let requests = node.triage.requests.lock().unwrap();
                    if !requests.contains_key(&request_id) {
                        return node
                            .triage
                            .answered
                            .lock()
                            .unwrap()
                            .get(&request_id)
                            .cloned();
                    }
                }
            });

            let answered = node
                .triage_command(json!({"action":"respond","request_id":request_id,
                    "result":{"behavior":"allow"}}))
                .await?;
            let seen = seen.join().unwrap().context("answered")?;
            assert_eq!(
                (seen["answer"].clone(), seen.get("sending")),
                (json!({"behavior":"allow"}), None),
                "a hook's request is never seen gone and not yet answered"
            );
            assert_eq!(
                (answered["answered"].clone(), answered["by"].clone()),
                (json!(true), json!("agent"))
            );
            assert_eq!(waiting.await?, json!({"behavior":"allow"}));
            assert_eq!(
                node.wait_request(&request_id).await,
                json!({"behavior":"allow"}),
                "a hook that comes to wait late still gets the answer"
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
    async fn an_unanswered_hook_request_goes_back_to_its_session() -> Result<()> {
        let (node, admin, dir) = node().await?;
        let result = async {
            with_agent(&node);
            bot(&node, &admin, "c", "前端", &[]).await?;
            let held = node.ingest_agent_event(asks_permission("s1")).await?;
            let request_id = held["request_id"]
                .as_str()
                .context("a held request")?
                .to_owned();
            node.triage
                .requests
                .lock()
                .unwrap()
                .get_mut(&request_id)
                .context("held")?
                .answer_by_ms = Some(now_ms() + 50);
            assert_eq!(
                node.wait_request(&request_id).await,
                json!({"decision":null})
            );
            assert_eq!(
                node.triage_command(json!({"action":"requests"})).await?["requests"],
                json!([])
            );
            let late = node
                .triage_command(json!({"action":"respond","request_id":request_id,
                    "result":{"behavior":"allow"}}))
                .await?;
            assert_eq!(
                (late["decision"].clone(), late["by"].clone()),
                (json!("undecided"), json!("fallback")),
                "a late answer gets the first one"
            );
            Ok::<_, anyhow::Error>(())
        }
        .await;
        cleanup(node, dir).await?;
        result
    }

    #[tokio::test]
    async fn an_answer_after_the_hook_left_is_not_reported_as_given() -> Result<()> {
        let (node, admin, dir) = node().await?;
        let result = async {
            with_agent(&node);
            bot(&node, &admin, "c", "前端", &[]).await?;
            let held = node.ingest_agent_event(asks_permission("s1")).await?;
            let request_id = held["request_id"]
                .as_str()
                .context("a held request")?
                .to_owned();
            let waiting = tokio::spawn({
                let node = node.clone();
                let request_id = request_id.clone();
                async move { node.wait_request(&request_id).await }
            });
            tokio::task::yield_now().await;
            waiting.abort();
            let _ = waiting.await;
            let answered = node
                .triage_command(json!({"action":"respond","request_id":request_id,
                    "result":{"behavior":"allow"}}))
                .await?;
            assert_eq!(
                (answered["answered"].clone(), answered["error"].clone()),
                (
                    json!(false),
                    json!("the session no longer waits for this request")
                )
            );
            Ok::<_, anyhow::Error>(())
        }
        .await;
        cleanup(node, dir).await?;
        result
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn answers_given_at_once_all_get_the_first() -> Result<()> {
        let (node, admin, dir) = node().await?;
        let result = async {
            with_agent(&node);
            bot(&node, &admin, "c", "前端", &[]).await?;
            for session in 0..20 {
                let held = node
                    .ingest_agent_event(asks_permission(&format!("s{session}")))
                    .await?;
                let request_id = held["request_id"]
                    .as_str()
                    .context("a held request")?
                    .to_owned();
                let start = Arc::new(tokio::sync::Barrier::new(4));
                let answers: Vec<_> = (0..4)
                    .map(|_| {
                        let (node, request_id, start) =
                            (node.clone(), request_id.clone(), start.clone());
                        tokio::spawn(async move {
                            start.wait().await;
                            let result = Some(Ok(json!({"behavior":"allow"})));
                            node.answer_request(&request_id, result, "agent").await
                        })
                    })
                    .collect();
                for answer in answers {
                    assert_eq!(answer.await??["by"], json!("agent"));
                }
            }
            Ok::<_, anyhow::Error>(())
        }
        .await;
        cleanup(node, dir).await?;
        result
    }

    #[tokio::test]
    async fn a_pi_session_whose_turn_ended_gets_its_next_message_or_nothing() -> Result<()> {
        let (node, admin, dir) = node().await?;
        let result = async {
            with_agent(&node);
            bot(&node, &admin, "c", "前端", &[]).await?;
            let ended = |turn: &str| {
                triage::from_pi_extension(&json!({"session_id":"p1","turn_id":turn,"cwd":"/w",
                    "stop_reason":"stop","last_message":"Tests written."}))
                .context("a turn end")
            };
            let held = node.ingest_agent_event(ended("e1")?).await?;
            let request_id = held["request_id"]
                .as_str()
                .context("a held request")?
                .to_owned();
            let listed =
                node.triage_command(json!({"action":"requests"})).await?["requests"][0].clone();
            assert_eq!(
                (listed["provider"].clone(), listed["kind"].clone()),
                (json!("pi"), json!("continue"))
            );
            let waiting = tokio::spawn({
                let node = node.clone();
                let request_id = request_id.clone();
                async move { node.wait_request(&request_id).await }
            });
            tokio::task::yield_now().await;
            node.triage_command(json!({"action":"respond","request_id":request_id,
                "result":{"text":"Now run them."}}))
                .await?;
            assert_eq!(waiting.await?, json!({"text":"Now run them."}));

            let held = node.ingest_agent_event(ended("e2")?).await?;
            let request_id = held["request_id"]
                .as_str()
                .context("a held request")?
                .to_owned();
            node.triage
                .requests
                .lock()
                .unwrap()
                .get_mut(&request_id)
                .context("held")?
                .answer_by_ms = Some(now_ms() + 50);
            assert_eq!(
                node.wait_request(&request_id).await,
                json!({"decision":null}),
                "nothing is sent to a session nobody answered"
            );
            Ok::<_, anyhow::Error>(())
        }
        .await;
        cleanup(node, dir).await?;
        result
    }

    #[tokio::test]
    async fn an_opencode_permission_gets_its_reply_or_goes_back_to_the_session() -> Result<()> {
        let (node, admin, dir) = node().await?;
        let result = async {
            with_agent(&node);
            bot(&node, &admin, "c", "前端", &[]).await?;
            let asks = |request: &str| {
                triage::from_opencode_plugin(&json!({"event":"permission.asked",
                    "session_id":"ses_1","request_id":request,"cwd":"/w","permission":"bash"}))
                .context("a permission request")
            };
            let held = node.ingest_agent_event(asks("per_1")?).await?;
            let request_id = held["request_id"]
                .as_str()
                .context("a held request")?
                .to_owned();
            let listed =
                node.triage_command(json!({"action":"requests"})).await?["requests"][0].clone();
            assert_eq!(
                (listed["provider"].clone(), listed["kind"].clone()),
                (json!("opencode"), json!("permission"))
            );
            let waiting = tokio::spawn({
                let node = node.clone();
                let request_id = request_id.clone();
                async move { node.wait_request(&request_id).await }
            });
            tokio::task::yield_now().await;
            node.triage_command(json!({"action":"respond","request_id":request_id,
                "result":{"reply":"always"}}))
                .await?;
            assert_eq!(waiting.await?, json!({"reply":"always"}));

            let held = node.ingest_agent_event(asks("per_2")?).await?;
            let request_id = held["request_id"]
                .as_str()
                .context("a held request")?
                .to_owned();
            node.triage
                .requests
                .lock()
                .unwrap()
                .get_mut(&request_id)
                .context("held")?
                .answer_by_ms = Some(now_ms() + 50);
            assert_eq!(
                node.wait_request(&request_id).await,
                json!({"decision":null}),
                "an unanswered request is left to the user"
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
    async fn native_request_questions_use_the_real_work_turn_without_merging_requests() -> Result<()>
    {
        let (node, admin, dir) = node().await?;
        let result = async {
            with_agent(&node);
            bot(&node, &admin, "c", "Native target", &[]).await?;
            running_work(&node, &admin, "native-target-work").await?;
            let network = node.network_id().await?;
            for item in ["first", "second"] {
                node.on_codex_request(&handed(
                    71,
                    "item/commandExecution/requestApproval",
                    json!({"threadId":"t-9","turnId":"u-1","itemId":item,"command":"fixture"}),
                ))
                .await;
            }
            let events = node.triage_command(json!({"action":"events"})).await?["events"]
                .as_array()
                .context("native events")?
                .clone();
            assert_eq!(events.len(), 2);
            assert_ne!(events[0]["event_id"], events[1]["event_id"]);
            assert_ne!(events[0]["request_id"], events[1]["request_id"]);
            let mut questions = Vec::new();
            for event in events {
                assert_eq!(
                    event["turn_id"],
                    format!("request:{}", event["request_id"].as_str().context("request ID")?),
                    "the event keeps its per-request deduplication key"
                );
                let raised = node
                    .triage_command(json!({"action":"raise","event_id":event["event_id"],
                        "severity":"urgent","question":"allow fixture?","request_id":event["request_id"]}))
                    .await?;
                let card = node.store.lock().await.question(
                    &network,
                    raised["question_id"].as_str().context("native question")?,
                )?.context("saved native question")?;
                assert_eq!(card["target"], json!({"kind":"work","work_id":"native-target-work"}));
                assert_eq!(card["about"]["request_id"], event["request_id"]);
                questions.push(card["question_id"].clone());
            }
            assert_ne!(questions[0], questions[1], "two items in one turn ask independently");
            assert_eq!(unanswered(&node).await?.len(), 2);
            Ok::<_, anyhow::Error>(())
        }.await;
        cleanup(node, dir).await?;
        result
    }

    #[tokio::test]
    async fn native_question_targets_require_matching_held_sources_and_keep_hook_turns()
    -> Result<()> {
        for case in [
            "missing-held",
            "missing-event-request",
            "missing-about-request",
            "wrong-held-id",
            "wrong-event-request",
            "wrong-about-request",
            "provider",
            "thread",
            "node",
            "missing-turn",
            "unknown-turn",
            "transport",
            "hook",
            "held-hook",
        ] {
            let (node, admin, dir) = node().await?;
            let result = async {
                with_agent(&node);
                bot(&node, &admin, "c", "Native source", &[]).await?;
                running_work(&node, &admin, "native-source-work").await?;
                let network = node.network_id().await?;
                node.on_codex_request(&handed(
                    72,
                    "item/commandExecution/requestApproval",
                    json!({"threadId":"t-9","turnId":"u-1","itemId":"source","command":"fixture"}),
                ))
                .await;
                let mut event: AgentEvent = serde_json::from_value(
                    node.triage_command(json!({"action":"events"})).await?["events"][0].clone(),
                )?;
                let id = event.request_id.clone().context("held request")?;
                let mut about = subject(&event, Some(id.clone()));
                {
                    let mut requests = lock(&node.triage.requests);
                    let request = requests.get_mut(&id).context("saved request")?;
                    match case {
                        "missing-event-request" => event.request_id = None,
                        "missing-about-request" => about.request_id = None,
                        "wrong-held-id" => request.request_id = "another-request".into(),
                        "wrong-event-request" => event.request_id = Some("another-request".into()),
                        "wrong-about-request" => about.request_id = Some("another-request".into()),
                        "provider" => request.provider = Provider::Claude,
                        "thread" => request.session_id = "another-thread".into(),
                        "node" => event.node = LocalIdentity::generate().device_id()?,
                        "missing-turn" => request.session_turn.clear(),
                        "unknown-turn" => request.session_turn = "unknown-turn".into(),
                        "transport" => {
                            request.reply = Reply::Hook {
                                answer: tokio::sync::watch::channel(None).0,
                                waited: false,
                            }
                        }
                        "hook" | "held-hook" => {
                            event.source = triage::Source::Hook;
                            event.turn_id = "u-1".into();
                            request.session_turn = "not-the-hook-turn".into();
                            if case == "hook" {
                                event.request_id = None;
                                about.request_id = None;
                            } else {
                                request.reply = Reply::Hook {
                                    answer: tokio::sync::watch::channel(None).0,
                                    waited: false,
                                };
                            }
                        }
                        _ => {}
                    }
                    if case == "missing-held" {
                        requests.remove(&id);
                    }
                }
                let question_id = format!("q-native-source-{case}");
                let raised = node
                    .raise(
                        &network,
                        &event,
                        EventBody::QuestionAsked {
                            question_id: question_id.clone(),
                            target: None,
                            attention: AttentionRequest {
                                kind: AttentionKind::Question,
                                device_id: Some(event.node.clone()),
                            },
                            severity: Severity::Attention,
                            text: "fixture source?".into(),
                            choices: vec![],
                            about,
                        },
                    )
                    .await;
                if matches!(case, "hook" | "held-hook") {
                    raised?;
                    let card = node
                        .store
                        .lock()
                        .await
                        .question(&network, &question_id)?
                        .context("hook question")?;
                    assert_eq!(
                        card["target"],
                        json!({"kind":"work","work_id":"native-source-work"}),
                        "{case}"
                    );
                } else if raised.is_ok() {
                    let card = node
                        .store
                        .lock()
                        .await
                        .question(&network, &question_id)?
                        .context("unscoped question")?;
                    assert!(
                        card["target"].is_null(),
                        "{case}: mismatched evidence cannot borrow a Work or Main target"
                    );
                }
                Ok::<_, anyhow::Error>(())
            }
            .await;
            cleanup(node, dir).await?;
            result.with_context(|| case)?;
        }
        Ok(())
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
            node.expire_requests().await?;
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
            assert!(
                jobs.is_empty(),
                "a held answer does not start a scheduler turn"
            );
            node.expire_requests().await?;
            let held =
                node.triage_command(json!({"action":"requests"})).await?["requests"][0].clone();
            assert!(
                held["answer_by_ms"].as_i64().is_some(),
                "the stored answer starts the control deadline"
            );
            let respond =
                json!({"action":"respond","request_id":request_id,"result":{"decision":"decline"}});
            assert!(
                node.triage_command(respond).await.is_err(),
                "a Bot cannot substitute its own decision for the stored answer"
            );
            node.pending_provider_launches.store(true, Ordering::SeqCst);
            node.wake_agent().await?;
            node.wake_agent().await?;
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

    #[tokio::test]
    async fn a_live_workspace_task_gets_its_one_approval_without_a_slot_or_another_turn()
    -> Result<()> {
        use crate::workspace::{
            InputIntent, InputRequest, InputRoute, InputTarget, WORKSPACE_SCOPE, WorkspaceCommand,
        };
        let (node, admin, dir) = node().await?;
        let result = async {
            with_agent(&node);
            {
                let mut settings = node.settings.lock().unwrap();
                settings.roots = vec![dir.canonicalize()?];
                settings.manager.as_mut().context("manager")?.workspace = dir.canonicalize()?;
            }
            bot(&node, &admin, "c", "Task Bot", &[]).await?;
            let network = node.network_id().await?;
            let command = WorkspaceCommand::CreateTask {
                task_id: "held-task".into(), channel_id: None, parent_task_id: None,
                owner_bot: "c".into(), title: "held task".into(), request: "do the task".into(),
                expected_result: "result".into(), source_events: vec![], dependencies: vec![], reply_only: false,
            };
            let signed = admin.attest(&network, WORKSPACE_SCOPE, EventBody::WorkspaceCommanded {
                command_id: "create-held-task".into(), command: command.clone(), acting_assignment: None,
                attestation: None, cause: None,
            })?;
            let target = InputTarget::Task { task_id: "held-task".into() };
            let input = InputRequest {
                message_id: "task-input".into(), route: InputRoute { target: target.clone(), intent: InputIntent::Continue,
                    sequence: 1, expected_revision: Some(1) }, text: "do the task".into(), attachments: vec![], report_results: Some(true),
            };
            let signed_input = admin.attest(&network, "c", EventBody::InputPosted {
                message_id: input.message_id.clone(), target: target.clone(), intent: input.route.intent,
                sequence: 1, expected_revision: Some(1), text: input.text.clone(), attachments: vec![],
                report_results: Some(true), attestation: None,
            })?;
            let (trigger, task_attempt, task_job) = {
                let mut store = node.store.lock().await;
                store.apply_workspace_command(&node.identity, &network, "create-held-task", command,
                    signed.attestation().cloned(), None, None, now_ms())?;
                store.post_workspace_input(&node.identity, &network, input,
                    signed_input.attestation().cloned().context("signed input")?, now_ms())?;
                let assignment = store.ensure_workspace_assignment(&node.identity, &network, "held-task",
                    &[dir.canonicalize()?.display().to_string()], now_ms())?.context("Task assignment")?;
                assert_eq!(assignment.trigger_message_id.as_deref(), Some("task-input"));
                let claimed = store.claim_next_fixture(&node.identity, &network, "c", &node.boot, "fixture")?.context("Task claim")?;
                assert_eq!(claimed.job.scope, crate::scheduling::SessionScope::WorkspaceTask { task_id: "held-task".into() });
                let trigger = claimed.job.trigger.clone();
                let attempt = claimed.job.state.attempt().context("Task attempt")?.id.clone();
                let job = store.mark_started("c", &trigger, &attempt)?;
                store.save_scheduler_native_id("c", &trigger, &attempt, "t-1", "held-task-token", true)?;
                (trigger, attempt, job)
            };
            let (mut server, handed) = app_server(&node).await?;
            let native_thread = handed.params["threadId"].as_str().context("native Task thread")?;
            let native_turn = handed.params["turnId"].as_str().context("native Task turn")?;
            let native = crate::scheduler::turn_recorder(&node, &network, &task_job, "system",
                Arc::new(std::sync::Mutex::new(Default::default())), None);
            (native.record)(native_thread.into(), Some(native_turn.into())).await?;
            native.started.context("Task native start callback")?(native_turn.into()).await?;
            let _cancel = node.register_manager_attempt_fixture("held-task-token".into(), Access {
                network_id: network.clone(), human: false, conversation_id: Some("c".into()), manager: None,
            }, &task_attempt, &trigger, Provider::Codex).await?;
            node.on_codex_request(&handed).await;
            let (question, request_id) = raise_for(&node).await?;
            let card = node.store.lock().await.question(&network, &question)?.context("approval")?;
            assert_eq!(card["target"], serde_json::to_value(&target)?);
            assert_eq!(card["choices"][0], json!({"label":"允许本次","action":"allow_once"}));
            assert!(signed_answer_fact(&node, &admin, &question, "继续").await.is_err());
            assert!(node.post_answer(&network, &question, "允许本次", None, AnswerAuthority::Signed).await.is_err());
            assert!(node.store.lock().await.message(&network, "c", &crate::store::questions::answer_id(&question))?.is_none());
            let answered = tokio::time::timeout(Duration::from_secs(3), signed_answer_fact(&node, &admin, &question, "允许本次")).await??;
            let retry = signed_answer_fact(&node, &admin, &question, "允许本次").await?;
            assert_eq!(answered["message"], retry["message"]);
            assert!(node.triage_command(json!({"action":"respond","request_id":request_id,"result":{"decision":"accept"}})).await.is_err());
            node.pending_provider_launches.store(true, Ordering::SeqCst);
            node.wake_agent().await?;
            assert_eq!(answered_once(&mut server).await["result"], json!({"decision":"accept"}));
            assert!(node.pending_provider_launches.load(Ordering::SeqCst), "an active Task keeps the ordinary launch change pending");
            {
                let store = node.store.lock().await;
                let jobs = store.scheduler_jobs("c")?;
                assert_eq!(jobs.len(), 1, "the stored approval creates no Task or Main turn");
                assert_eq!(jobs[0].state.name(), "running", "the original Task remains active");
                assert!(store.held_answer_receipt(&network, "c", answered["message"]["message_id"].as_str().unwrap())?.is_some());
            }
            let local = node.identity.device_id()?;
            let ordinary = EventBody::QuestionAsked {
                question_id: "q-ordinary-task-question".into(), target: Some(target.clone()),
                attention: AttentionRequest { kind: AttentionKind::Question, device_id: Some(local.clone()) },
                severity: Severity::Info, text: "what next?".into(), choices: vec![],
                about: Subject { node: local.clone(), agent: Some(Provider::Codex), session_id: "t-1".into(),
                    cwd: String::new(), summary: String::new(), request_id: None },
            };
            node.store.lock().await.ask(&node.identity, &network, "c", ordinary, now_ms())?;
            signed_answer_fact(&node, &admin, "q-ordinary-task-question", "next step").await?;
            let next = {
                let store = node.store.lock().await;
                let jobs = store.scheduler_jobs("c")?;
                let next = jobs.iter().find(|job| job.kind == crate::scheduling::TriggerKind::Answer).context("ordinary Answer")?.clone();
                assert_eq!(next.scope, crate::scheduling::SessionScope::WorkspaceTask { task_id: "held-task".into() });
                assert!(store.scheduler_job_hold(&network, &local, &next, Provider::Codex,
                    &crate::config::SchedulingLimits { machine_slots: 1, task_limit: 1 })?.is_some(), "the occupied scheduler cannot start the ordinary follow-up");
                next
            };
            node.finish_manager_attempt(&task_attempt).await;
            let mut store = node.store.lock().await;
            store.finish_job(&node.identity, &network, "c", &trigger, &task_attempt,
                crate::scheduling::JobOutcome::Completed, "done", now_ms())?;
            assert_eq!(store.claim_next_fixture(&node.identity, &network, "c", &node.boot, "fixture")?.context("ordinary follow-up")?.job.trigger, next.trigger);
            Ok::<_, anyhow::Error>(())
        }.await;
        cleanup(node, dir).await?;
        result
    }

    async fn human_workspace_command(
        node: &Runtime,
        admin: &LocalIdentity,
        id: &str,
        command: crate::workspace::WorkspaceCommand,
    ) -> Result<()> {
        let network = node.network_id().await?;
        let signed = admin.attest(
            &network,
            crate::workspace::WORKSPACE_SCOPE,
            EventBody::WorkspaceCommanded {
                command_id: id.into(),
                command: command.clone(),
                acting_assignment: None,
                attestation: None,
                cause: None,
            },
        )?;
        node.workspace_command(
            &Access {
                network_id: network,
                human: true,
                conversation_id: None,
                manager: None,
            },
            &json!({"command_id":id,"command":command,"attestation":signed.attestation()}),
        )
        .await?;
        Ok(())
    }

    #[tokio::test]
    async fn held_work_approvals_recheck_current_assignment_and_hold_the_input_lease() -> Result<()>
    {
        use crate::workspace::{AssignmentState, TaskAction, WorkspaceCommand};
        use futures_util::SinkExt as _;
        for (change, deny) in [
            ("standalone", false),
            ("shared", false),
            ("shared-stopped", false),
            ("shared-scope", false),
            ("assigned", false),
            ("pause", false),
            ("pause", true),
            ("cancel", false),
            ("cancel", true),
            ("revise", false),
            ("revise", true),
            ("retarget", false),
            ("retarget", true),
            ("native-cwd", false),
            ("native-cwd", true),
        ] {
            let (node, admin, dir) = node().await?;
            let mut socket_task = None;
            let result = async {
                with_agent(&node);
                let root = dir.canonicalize()?;
                let directory = root.join("work");
                std::fs::create_dir(&directory)?;
                {
                    let mut settings = node.settings.lock().unwrap();
                    settings.roots = vec![root.clone()];
                    settings.manager.as_mut().context("manager")?.workspace = directory.clone();
                }
                bot(&node, &admin, "c", "Task Bot", &[]).await?;
                let network = node.network_id().await?;
                let local = node.identity.device_id()?;
                let (mut socket, handed) = app_server(&node).await?;
                let (sent, mut received) = tokio::sync::mpsc::unbounded_channel();
                let native_directory = Arc::new(std::sync::Mutex::new(directory.display().to_string()));
                let served_directory = native_directory.clone();
                let native_work_ids = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
                let served_work_ids = native_work_ids.clone();
                socket_task = Some(tokio::spawn(async move {
                    loop {
                        let message = read(&mut socket).await;
                        if message["id"] == 41 && message.get("result").is_some() {
                            let _ = sent.send(message);
                        } else if message.get("id").is_some() && message.get("method").is_some() {
                            let result = match message["method"].as_str() {
                                Some("thread/read" | "thread/resume") => {
                                    json!({"thread":{"id":"t-1","cwd":lock(&served_directory).clone(),
                                    "turns":[{"id":"u-1","status":"inProgress","items":lock(&served_work_ids).iter().map(|id| json!({"type":"userMessage","clientId":id})).collect::<Vec<_>>()}]}})
                                }
                                _ => json!({}),
                            };
                            if socket
                                .send(
                                    json!({"id":message["id"],"result":result})
                                        .to_string()
                                        .into(),
                                )
                                .await
                                .is_err()
                            {
                                break;
                            }
                        }
                    }
                }));
                let trigger = if change == "standalone" {
                    main_input_fact(&node, &admin, "c", "held-work-origin").await?
                } else {
                    human_workspace_command(
                        &node,
                        &admin,
                        "create-held-source",
                        WorkspaceCommand::CreateTask {
                            task_id: "held-source".into(),
                            channel_id: None,
                            parent_task_id: None,
                            owner_bot: "c".into(),
                            title: "source".into(),
                            request: "perform the task".into(),
                            expected_result: "result".into(),
                            source_events: vec![],
                            dependencies: vec![],
                            reply_only: false,
                        },
                    )
                    .await?;
                    let epoch = node
                        .store
                        .lock()
                        .await
                        .conversation(&network, "c")?
                        .context("Bot")?
                        .owner_epoch;
                    human_workspace_command(
                        &node,
                        &admin,
                        "assign-held-source",
                        WorkspaceCommand::AssignTask {
                            task_id: "held-source".into(),
                            expected_revision: 1,
                            assignment_id: "held-assignment".into(),
                            bot_id: "c".into(),
                            execution_node: local.clone(),
                            bot_owner_epoch: epoch,
                            channel_owner_epoch: None,
                            work_id: None,
                            resource_roots: vec![directory.display().to_string()],
                            automatic: false,
                        },
                    )
                    .await?;
                    node.store.lock().await.apply_workspace_command(
                        &node.identity,
                        &network,
                        "accept-held-source",
                        WorkspaceCommand::AssignmentProgress {
                            execution: None,
                            assignment_id: "held-assignment".into(),
                            revision: 1,
                            state: AssignmentState::Accepted,
                            input_event: None,
                            evidence_event: None,
                            result: None,
                        },
                        None,
                        None,
                        None,
                        now_ms(),
                    )?;
                    "assignment:held-assignment".into()
                };
                let work_id = crate::work::WorkSpec::id("c", &trigger, "held-child");
                let spec = crate::work::WorkSpec {
                    work_id: work_id.clone(),
                    conversation: "c".into(),
                    trigger,
                    target: local.clone(),
                    dispatcher: local,
                    call_ref: "held-child".into(),
                    kind: crate::model::WorkKind::Agent(Provider::Codex),
                    model: None,
                    resolved: ResolvedModel {
                        provider: Provider::Codex,
                        model: None,
                    },
                    cwd: if change == "native-cwd" {
                        String::new()
                    } else {
                        directory.display().to_string()
                    },
                    title: "child".into(),
                    prompt: "do the accepted task".into(),
                    continues: None,
                    parent: None,
                    thread: Some("t-1".into()),
                };
                node.store.lock().await.accept_work_fixture(
                    &node.identity,
                    &network,
                    spec.clone(),
                    now_ms(),
                )?;
                drop(node.work_start_lease(&network, &spec, Some("t-1")).await?);
                node.signal_work(
                    &network,
                    &work_id,
                    crate::work::Signal::TurnStarted {
                        turn_id: "u-1".into(),
                        epoch: node.codex.epoch(),
                    },
                )
                .await?;
                lock(&native_work_ids).push(work_id.clone());
                let mut group_ids = vec![work_id.clone()];
                if change.starts_with("shared") {
                    let mut second = spec.clone();
                    second.call_ref = "held-second".into();
                    second.work_id = crate::work::WorkSpec::id("c", &second.trigger, &second.call_ref);
                    if change == "shared-scope" { second.parent = Some("another-task".into()); }
                    node.store.lock().await.accept_work_fixture(&node.identity, &network, second.clone(), now_ms())?;
                    if change != "shared-scope" { drop(node.work_start_lease(&network, &second, Some("t-1")).await?); }
                    node.signal_work(&network, &second.work_id, crate::work::Signal::TurnStarted { turn_id:"u-1".into(), epoch:node.codex.epoch() }).await?;
                    lock(&native_work_ids).push(second.work_id.clone());
                    group_ids.push(second.work_id);
                }
                {
                    let store = node.store.lock().await;
                    assert_eq!(store.work_native_write_allowed(&network, &work_id)?, change != "shared-scope");
                    assert!(
                        store
                            .work(&network, &work_id)?
                            .context("work")?
                            .parent
                            .is_none()
                    );
                }
                node.on_codex_request(&handed).await;
                let (question, request_id) = raise_for(&node).await?;
                signed_answer_fact(
                    &node,
                    &admin,
                    &question,
                    if deny { "拒绝" } else { "允许本次" },
                )
                .await?;
                if matches!(change, "standalone" | "assigned" | "shared") {
                    let attempt = {
                        let mut store = node.store.lock().await;
                        let TriageAdmission::Claimed(attempt) =
                            store.admit_triage(&network, &node.boot, BATCH, 100, now_ms())?
                        else {
                            bail!("standalone control claim");
                        };
                        attempt
                    };
                    let leases = node
                        .answer_permission_lease(attempt.answer().context("control")?)
                        .await?;
                    for id in &group_ids {
                        assert!(node.work_input_lock(id).await.try_write_owned().is_err(),
                            "the approval lease retains every shared Work input read lock");
                    }
                    drop(leases);
                    node.store.lock().await.finish_triage(
                        attempt,
                        &Err(manager::ManagerError::not_started(anyhow::anyhow!(
                            "lease checked before any transport write"
                        ))),
                    )?;
                    let write = node.work_input_lock(&work_id).await.write_owned().await;
                    let wake_node = node.clone();
                    let wake = tokio::spawn(async move { wake_node.wake_agent().await });
                    tokio::time::sleep(Duration::from_millis(50)).await;
                    assert!(
                        received.try_recv().is_err(),
                        "an existing Work writer fences the original transport response"
                    );
                    drop(write);
                    tokio::time::timeout(Duration::from_secs(5), wake).await???;
                    assert_eq!(
                        tokio::time::timeout(Duration::from_secs(5), received.recv())
                            .await?
                            .context("one response")?["result"],
                        json!({"decision":"accept"})
                    );
                } else {
                    if change == "shared-stopped" {
                        node.store.lock().await.save_stop_request(&crate::work::StopRequest {
                            work_id:group_ids[1].clone(), conversation:"c".into(), target:node.identity.device_id()?,
                            requested_at_ms:now_ms(), deadline_ms:now_ms()+1000, timed_out:false,
                        })?;
                    } else if change == "shared-scope" {
                    } else if matches!(change, "retarget" | "native-cwd") {
                        let replacement = root.join("other");
                        std::fs::create_dir(&replacement)?;
                        if change == "retarget" {
                            std::fs::rename(&directory, root.join("previous-work"))?;
                            vibeface_bridge::platform::privatefs::link_dir(&replacement, &directory)?;
                            assert_eq!(
                                node.allowed_path(directory.to_str().context("work directory")?)?,
                                replacement.canonicalize()?,
                                "the Node's broad root alone still permits the replaced path"
                            );
                        } else {
                            *lock(&native_directory) = replacement.display().to_string();
                        }
                    } else {
                        let command = match change {
                            "pause" | "cancel" => WorkspaceCommand::ControlTask {
                                task_id: "held-source".into(),
                                expected_revision: 1,
                                action: if change == "pause" {
                                    TaskAction::Stop
                                } else {
                                    TaskAction::Cancel
                                },
                            },
                            "revise" => WorkspaceCommand::ReviseTask {
                                task_id: "held-source".into(),
                                expected_revision: 1,
                                request: "new task revision".into(),
                                expected_result: "new result".into(),
                                dependencies: vec![],
                            },
                            _ => unreachable!(),
                        };
                        human_workspace_command(&node, &admin, "change-held-source", command).await?;
                    }
                    {
                        let store = node.store.lock().await;
                        if !matches!(change, "retarget" | "native-cwd") {
                            assert!(!store.work_native_write_allowed(&network, &work_id)?);
                        }
                        assert_eq!(store.stop_requests()?.is_empty(), change != "shared-stopped",
                            "only the shared-stop fixture has sent a stop request");
                        assert!(
                            !store
                                .work_record(&work_id)?
                                .context("work")?
                                .state
                                .finished()
                        );
                    }
                    if deny {
                        node.wake_agent().await?;
                        assert_eq!(
                            tokio::time::timeout(Duration::from_secs(5), received.recv())
                                .await?
                                .context("one denial")?["result"],
                            json!({"decision":"decline"})
                        );
                    } else {
                        let error = node
                            .wake_agent()
                            .await
                            .expect_err("an obsolete assignment cannot approve its Work");
                        let reason = match change {
                            "shared-stopped" | "shared-scope" => "different native executions or work scopes",
                            "retarget" => "assignment resource roots",
                            "native-cwd" => "native directory differs from the reserved work",
                            _ => "current authority",
                        };
                        assert!(error.to_string().contains(reason), "{change}: {error:#}");
                        assert!(lock(&node.triage.requests).contains_key(&request_id));
                        assert!(lock(&node.triage.answered).is_empty());
                        assert!(received.try_recv().is_err());
                    }
                }
                assert!(
                    tokio::time::timeout(Duration::from_millis(50), received.recv())
                        .await
                        .is_err(),
                    "a control never sends another response"
                );
                Ok::<_, anyhow::Error>(())
            }
            .await;
            if let Some(task) = socket_task {
                task.abort();
            }
            cleanup(node, dir).await?;
            result?;
        }
        Ok(())
    }

    #[tokio::test]
    async fn answer_control_claims_reject_changed_bindings_and_unknown_never_replays() -> Result<()>
    {
        let (node, admin, dir) = node().await?;
        let before = async {
            with_agent(&node);
            bot(&node, &admin, "c", "Bot", &[]).await?;
            let held = node.ingest_agent_event(asks_permission("answer-control")).await?;
            let event = node.triage_command(json!({"action":"events"})).await?["events"][0].clone();
            let raised = node.triage_command(json!({"action":"raise","event_id":event["event_id"],
                "request_id":held["request_id"],"severity":"attention","question":"run?","choices":["model supplied approval"]})).await?;
            let question = raised["question_id"].as_str().context("question")?;
            let network = node.network_id().await?;
            signed_answer_fact(&node, &admin, question, "允许本次").await?;
            let attempt = {
                let mut store = node.store.lock().await;
                let TriageAdmission::Claimed(attempt) = store.admit_triage(&network, &node.boot, BATCH, 100, now_ms())? else { bail!("answer claim"); };
                assert_eq!(attempt.events().len(), 1, "an answer claim is never mixed with ordinary events");
                let original = attempt.answer().context("private answer binding")?;
                for changed in [
                    HeldAnswerControl { question_hash: "other".into(), ..original.clone() },
                    HeldAnswerControl { answer_hash: "other".into(), ..original.clone() },
                    HeldAnswerControl { conversation_id: "other".into(), ..original.clone() },
                ] {
                    assert!(store.validate_answer_control_claim(&node.boot, attempt.id(), &changed).is_err());
                }
                attempt
            };
            *lock(&node.triage.active) = Some(attempt.id().into());
            let base = json!({"_attempt":attempt.id(),"action":"respond","request_id":held["request_id"],"result":{"behavior":"allow"}});
            for changed in [
                json!({"_attempt":attempt.id(),"action":"raise","event_id":event["event_id"],"severity":"info","question":"other"}),
                json!({"_attempt":attempt.id(),"action":"dismiss","event_id":event["event_id"]}),
                json!({"_attempt":attempt.id(),"action":"events"}),
                json!({"_attempt":attempt.id(),"action":"respond","request_id":"another-request","result":{"behavior":"allow"}}),
                json!({"_attempt":attempt.id(),"action":"respond","node":"another-node","request_id":held["request_id"],"result":{"behavior":"allow"}}),
                json!({"_attempt":attempt.id(),"action":"respond","request_id":held["request_id"],"result":{"behavior":"allow","updatedInput":{"command":"other"}}}),
                json!({"_attempt":"old-claim","action":"respond","request_id":held["request_id"],"result":{"behavior":"allow"}}),
            ] { assert!(node.triage_command(changed).await.is_err()); }
            assert_eq!(node.triage_command(json!({"_attempt":attempt.id(),"action":"requests"})).await?["requests"].as_array().unwrap().len(), 1);
            let request_id = held["request_id"].as_str().context("request")?;
            let generation = {
                let mut requests = lock(&node.triage.requests);
                let request = requests.get_mut(request_id).context("held request")?;
                std::mem::replace(&mut request.generation, "replacement-transport".into())
            };
            assert!(node.triage_command(base.clone()).await.is_err(), "reconnection cannot borrow an older claim");
            lock(&node.triage.requests).get_mut(request_id).context("held request")?.generation = generation;
            node.local_permission_epoch.fetch_add(1, Ordering::AcqRel);
            assert!(node.triage_command(base.clone()).await.is_err(), "revoked permissions cannot approve a still-live request");
            let failure: Result<()> = Err(anyhow::anyhow!("the control outcome is unknown"));
            node.store.lock().await.finish_triage(attempt, &failure)?;
            assert!(node.triage_command(base).await.is_err(), "an unknown claim cannot resume sending");
            *lock(&node.triage.active) = None;
            node.wake_agent().await?;
            assert!(lock(&node.triage.answered).is_empty());
            assert_eq!(node.triage_status().await?["unknown_events"], 1);
            Ok::<_, anyhow::Error>((network, question.to_owned(), request_id.to_owned()))
        }.await;
        drop(node);
        let reopened = Arc::new(Runtime::reopen(&dir).await?);
        let result = async {
            let (network, question, request) = before?;
            with_agent(&reopened);
            assert!(lock(&reopened.triage.requests).is_empty());
            reopened.wake_agent().await?;
            assert_eq!(reopened.triage_status().await?["unknown_events"], 1);
            assert!(reopened.store.lock().await.held_answer_receipt(&network, "c", &crate::store::questions::answer_id(&question))?.is_some());
            assert!(reopened.triage_command(json!({"action":"respond","request_id":request,"result":{"behavior":"allow"}})).await.is_err());
            Ok::<_, anyhow::Error>(())
        }.await;
        cleanup(reopened, dir).await?;
        result
    }

    #[test]
    fn answer_results_use_the_original_question_schema_and_text() -> Result<()> {
        let schema = codex_answer_response(
            "item/tool/requestUserInput",
            &json!({"questions":[
            {"id":"database","question":"Which database?","options":[{"label":"SQLite"},{"label":"PostgreSQL"}]}]}),
        );
        assert!(
            validate_answer_result(
                &schema,
                &json!({}),
                "SQLite",
                &json!({"answers":{"database":{"answers":["SQLite"]}}})
            )
            .is_ok()
        );
        for result in [
            json!({"decision":"accept"}),
            json!({"answers":{"other":{"answers":["SQLite"]}}}),
            json!({"answers":{"database":{"answers":["PostgreSQL"]}}}),
            json!({"answers":{"database":{"answers":["SQLite"]}},"permissions":{"network":true}}),
        ] {
            assert!(validate_answer_result(&schema, &json!({}), "SQLite", &result).is_err());
        }
        let event = triage::from_claude_hook(&json!({"hook_event_name":"PermissionRequest","session_id":"session",
            "tool_name":"AskUserQuestion","tool_input":{"questions":[{"question":"Which database?","options":[{"label":"SQLite"}]}]}}), 10).context("event")?;
        let mut hook = HookRequest {
            provider: event.agent,
            session_id: event.session_id.clone(),
            turn_id: event.turn_id.clone(),
            kind: RequestKind::Question,
            questions: Some(json!([{"question":"Which database?","options":[{"label":"SQLite"}]}])),
        };
        let response = hook_answer_response(&event, Some(&hook))?;
        assert!(
            validate_answer_result(
                &response,
                &json!({}),
                "SQLite",
                &json!({"answers":{"Which database?":"SQLite"}})
            )
            .is_ok()
        );
        hook.session_id = "other-session".into();
        assert!(hook_answer_response(&event, Some(&hook)).is_err());
        Ok(())
    }

    /// The App Server asks `rpc` for the approval of item `i-1` in turn `u-1` of thread `t-1`;
    /// returns what the bridge hands the Node.
    async fn ask_approval(node: &Runtime, server: &mut Server, rpc: i64) -> Result<ServerRequest> {
        use futures_util::SinkExt as _;
        let params = json!({"threadId":"t-1","turnId":"u-1","itemId":"i-1","command":"rm -rf build","reason":"clean"});
        let method = "item/commandExecution/requestApproval";
        server
            .send(
                json!({"id":rpc,"method":method,"params":params})
                    .to_string()
                    .into(),
            )
            .await?;
        while !node.codex.client().snapshot().await["pending_requests"]
            .as_array()
            .is_some_and(|pending| pending.iter().any(|request| request["id"] == rpc))
        {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        Ok(handed(rpc, method, params))
    }

    async fn raise_for(node: &Runtime) -> Result<(String, String)> {
        let event = node.triage_command(json!({"action":"events"})).await?["events"][0].clone();
        let request_id = event["request_id"]
            .as_str()
            .context("request event")?
            .to_owned();
        let raised = node
            .triage_command(json!({"action":"raise","event_id":event["event_id"],
            "severity":"attention","question":"要清理 build 吗？","request_id":request_id,
            "choices":["允许","拒绝"]}))
            .await?;
        Ok((
            raised["question_id"]
                .as_str()
                .context("question")?
                .to_owned(),
            request_id,
        ))
    }

    #[tokio::test]
    async fn a_request_asked_again_keeps_its_one_question() -> Result<()> {
        let (node, admin, dir) = node().await?;
        let result = async {
            with_agent(&node);
            bot(&node, &admin, "c", "前端", &["t-1"]).await?;
            let (mut server, _) = app_server(&node).await?;
            let handed = ask_approval(&node, &mut server, 51).await?;
            node.on_codex_request(&handed).await;
            let (question, request_id) = raise_for(&node).await?;
            node.drop_codex_requests().await?;
            let again = ask_approval(&node, &mut server, 52).await?;
            node.on_codex_request(&again).await;
            let asked = unanswered(&node).await?;
            assert_eq!(asked.len(), 1, "one question for the request asked twice");
            assert_eq!(asked[0]["question_id"], question.as_str());
            let held = node.triage_command(json!({"action":"requests"})).await?["requests"].clone();
            assert_eq!(
                (
                    held[0]["request_id"].clone(),
                    held[0]["question_id"].clone(),
                    held[0]["answer_by_ms"].clone()
                ),
                (json!(request_id), json!(question), Value::Null),
                "the request asked again stays held by its question"
            );
            assert_eq!(
                node.triage_command(json!({"action":"events"})).await?["events"],
                json!([]),
                "the Agent is not asked again"
            );
            node.close_lost_questions().await?;
            assert_eq!(
                unanswered(&node).await?.len(),
                1,
                "a held request keeps its question open"
            );
            assert_eq!(answer(&node, &question, "允许本次").await?, "answered");
            node.wake_agent().await?;
            let sent = loop {
                let message = read(&mut server).await;
                if message.get("result").is_some() {
                    break message;
                }
            };
            assert_eq!(
                (sent["id"].clone(), sent["result"].clone()),
                (json!(52), json!({"decision":"accept"})),
                "the answer goes to the request as asked on the latest connection"
            );
            Ok::<_, anyhow::Error>(())
        }
        .await;
        cleanup(node, dir).await?;
        result
    }

    #[tokio::test]
    async fn a_question_about_a_lost_request_closes_and_refuses_answers() -> Result<()> {
        let (node, admin, dir) = node().await?;
        let result = async {
            with_agent(&node);
            bot(&node, &admin, "c", "前端", &["t-1"]).await?;
            let (mut server, _) = app_server(&node).await?;
            let handed = ask_approval(&node, &mut server, 61).await?;
            node.on_codex_request(&handed).await;
            let (question, _) = raise_for(&node).await?;
            node.drop_codex_requests().await?;
            node.close_lost_questions().await?;
            assert!(
                unanswered(&node).await?.is_empty(),
                "待处理 no longer lists it"
            );
            let network = node.network_id().await?;
            let chat = node
                .store
                .lock()
                .await
                .conversation_questions(&network, "c")?;
            assert_eq!(
                (chat[0]["question_id"].clone(), chat[0]["closed"].clone()),
                (json!(question), json!("request_lost"))
            );
            let refused = answer(&node, &question, "允许")
                .await
                .err()
                .context("refused")?;
            assert!(refused.to_string().contains("closed"), "{refused}");

            let late = ask_approval(&node, &mut server, 62).await?;
            node.on_codex_request(&late).await;
            let sent = loop {
                let message = read(&mut server).await;
                if message.get("result").is_some() {
                    break message;
                }
            };
            assert_eq!(
                (sent["id"].clone(), sent["result"].clone()),
                (json!(62), json!({"decision":"decline"})),
                "a request asked again after its question closed gets the fallback"
            );
            assert!(unanswered(&node).await?.is_empty(), "and no new question");
            Ok::<_, anyhow::Error>(())
        }
        .await;
        cleanup(node, dir).await?;
        result
    }

    fn turn(session: &str, turn: &str, cwd: &str) -> AgentEvent {
        AgentEvent {
            source: triage::Source::Hook,
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
        let private_root =
            std::env::temp_dir().join(format!("vibeface-private-event-{}", uuid::Uuid::new_v4()));
        let private_project = private_root.join("project");
        let private_project_text = private_project.to_string_lossy();
        let event = AgentEvent {
            host: "private-host".into(),
            repo: Some(private_project_text.to_string()),
            last_message: format!("failed in {private_project_text}/src"),
            ..turn("session", "turn", &private_project_text)
        };
        let shared = shared_event(&event, std::slice::from_ref(&private_root));
        assert_eq!(shared.cwd, "workspace-1/project");
        assert!(shared.host.is_empty() && shared.repo.is_none());
        assert!(
            !shared
                .last_message
                .contains(private_root.to_string_lossy().as_ref())
        );
        assert_eq!(event.cwd, private_project_text);
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
            node.register_manager_attempt_fixture("token".into(), idle.clone(), &attempt, &trigger, crate::providers::Provider::Codex)
                .await?;
            let live = node.agent_tokens.lock().await.get("token").cloned().context("registered tool token")?;
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

    async fn delete_bot(node: &Runtime, admin: &LocalIdentity, scope: &str) -> Result<String> {
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
        let written = node
            .store
            .lock()
            .await
            .delete_bot(&node.identity, &network, scope, signed, now_ms())?
            .context("deleted")?;
        Ok(written.hash)
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
    async fn a_deleted_bots_work_thread_goes_to_the_primary_bot() -> Result<()> {
        let (node, admin, dir) = node().await?;
        let result = async {
            with_agent(&node);
            bot(&node, &admin, "c", "前端", &[]).await?;
            bot(&node, &admin, "d", "后端", &[]).await?;
            let network = node.network_id().await?;
            let local = node.identity.device_id()?;
            let trigger = main_input_fact(&node, &admin, "d", "m1").await?;
            let spec = crate::work::WorkSpec {
                work_id: "w-d".into(),
                conversation: "d".into(),
                trigger,
                target: local.clone(),
                dispatcher: local,
                call_ref: "r".into(),
                kind: crate::model::WorkKind::Agent(crate::providers::Provider::Codex),
                model: None,
                resolved: crate::model::ResolvedModel { provider: crate::providers::Provider::Codex, model: None },
                cwd: "/w".into(),
                title: "t".into(),
                prompt: "p".into(),
                continues: None,
                parent: None,
                thread: Some("wt".into()),
            };
            node.store
                .lock()
                .await
                .accept_work_fixture(&node.identity, &network, spec, now_ms())?;
            let queued = node.ingest_agent_event(turn("wt", "t1", "/w")).await?;
            assert_eq!(queued["status"], "accepted");
            assert_eq!(attributions(&node, "d").await?, 0, "a work's thread is its bot's at once");
            node.bind_work(
                &network,
                "d",
                "w-d",
                vibeface_bridge::codex::StartReport::Rejected(
                    vibeface_bridge::failure::ExecFailure::new(
                        FailureKind::Rejected,
                        "native start was rejected before Bot deletion",
                    ),
                ),
            )
            .await?;
            assert_eq!(
                state(&node, "w-d").await?,
                crate::work::WorkState::Finished {
                    outcome: crate::model::WorkOutcome::Rejected,
                }
            );
            delete_bot(&node, &admin, "d").await?;

            let raise = json!({"action":"raise","event_id":queued["event_id"],"severity":"attention","question":"测试失败，要修吗？"});
            assert_eq!(
                node.triage_command(raise).await?["conversation_id"], "c",
                "a deleted bot's work thread asks in the primary bot's chat"
            );
            let next = node.ingest_agent_event(turn("wt", "t2", "/w")).await?;
            assert_eq!(next["status"], "accepted");
            assert_eq!(owner_of(&node, "wt").await?.as_deref(), Some("c"));
            assert_eq!(attributions(&node, "c").await?, 1, "the thread goes to the primary bot once");
            Ok::<_, anyhow::Error>(())
        }
        .await;
        cleanup(node, dir).await?;
        result
    }

    #[tokio::test]
    async fn deleting_a_bot_stops_the_works_it_dispatched() -> Result<()> {
        let (node, admin, dir) = node().await?;
        let result = async {
            with_agent(&node);
            bot(&node, &admin, "c", "前端", &[]).await?;
            bot(&node, &admin, "d", "后端", &[]).await?;
            let network = node.network_id().await?;
            let local = node.identity.device_id()?;
            let spec = |work_id: &str, conversation: &str| crate::work::WorkSpec {
                work_id: work_id.into(),
                conversation: conversation.into(),
                trigger: "message:m1".into(),
                target: local.clone(),
                dispatcher: local.clone(),
                call_ref: "r".into(),
                kind: crate::model::WorkKind::Agent(crate::providers::Provider::Codex),
                model: None,
                resolved: crate::model::ResolvedModel {
                    provider: crate::providers::Provider::Codex,
                    model: None,
                },
                cwd: "/w".into(),
                title: "t".into(),
                prompt: "p".into(),
                continues: None,
                parent: None,
                thread: Some(format!("t-{work_id}")),
            };
            for (work_id, conversation) in [("w-d", "d"), ("w-c", "c")] {
                node.store.lock().await.accept_work_fixture(
                    &node.identity,
                    &network,
                    spec(work_id, conversation),
                    now_ms(),
                )?;
            }
            let hash = delete_bot(&node, &admin, "d").await?;
            node.bot_deleted(&network, "d", &hash);
            let late = node.store.lock().await.accept_work_fixture(
                &node.identity,
                &network,
                spec("w-late", "d"),
                now_ms(),
            );
            assert!(
                late.is_err(),
                "a dispatch that arrives after the deletion does not start"
            );
            let stopping = |requests: &[crate::work::StopRequest], work: &str| {
                requests.iter().any(|request| request.work_id == work)
            };
            let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
            let held = loop {
                let held = node.store.lock().await.stop_requests()?;
                if stopping(&held, "w-d") || tokio::time::Instant::now() > deadline {
                    break held;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            };
            assert!(
                stopping(&held, "w-d"),
                "the deleted bot's work is being stopped"
            );
            assert!(!stopping(&held, "w-c"), "another bot's work keeps running");
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

    #[tokio::test]
    async fn app_server_turns_recover_only_their_missing_thread_metadata() -> Result<()> {
        for case in [
            "missing-start",
            "known",
            "wrong-thread",
            "missing-thread",
            "missing-cwd",
            "invalid-cwd",
            "empty-cwd",
            "oversized-cwd",
            "failed-read",
            "background",
        ] {
            let (node, admin, dir) = node().await?;
            let result = async {
                with_agent(&node);
                bot(&node, &admin, "c", "Metadata", &[]).await?;
                let mut queries = scripted_app_server(&node, move |request| {
                    if request["method"] != "thread/read" {
                        return json!({"result":{}});
                    }
                    let mut thread = json!({"id":"metadata-thread","cwd":"/native/project","source":"cli"});
                    match case {
                        "wrong-thread" => {
                            thread["id"] = json!("another-thread");
                            thread["source"] = json!({"subAgent":"review"});
                        }
                        "missing-thread" => thread = Value::Null,
                        "missing-cwd" => { thread.as_object_mut().unwrap().remove("cwd"); }
                        "invalid-cwd" => thread["cwd"] = json!(42),
                        "empty-cwd" => thread["cwd"] = json!(""),
                        "oversized-cwd" => thread["cwd"] = json!("x".repeat(4097)),
                        "failed-read" => return json!({"error":{"code":-32000,"message":"unreadable"}}),
                        "background" => thread["source"] = json!({"subAgent":"review"}),
                        _ => {}
                    }
                    json!({"result":{"thread":thread}})
                }).await?;
                tokio::time::timeout(Duration::from_secs(5), connected(&node)).await?;
                if case == "known" {
                    node.triage_app_server(&json!({"method":"thread/started","params":{"thread":
                        {"id":"metadata-thread","cwd":"/known/project","source":"cli"}}}));
                }
                node.triage_app_server(&json!({"method":"turn/completed","params":{
                    "threadId":"metadata-thread","turn":{"id":"metadata-turn","status":"completed","items":[]}}}));
                let queued = tokio::time::timeout(Duration::from_secs(5), async {
                    loop {
                        if case == "background" && lock(&node.triage.background).contains("metadata-thread") {
                            return Ok::<Option<AgentEvent>, anyhow::Error>(None);
                        }
                        if let Some(event) = node.store.lock().await.triage_events(None)?.into_iter().next() {
                            return Ok(Some(event));
                        }
                        tokio::task::yield_now().await;
                    }
                }).await??;
                if case == "background" {
                    assert!(queued.is_none());
                    assert_eq!(owner_of(&node, "metadata-thread").await?, None);
                    assert_eq!(node.ingest_agent_event(turn("metadata-thread", "metadata-turn", "/native/project")).await?["status"], "foreign");
                } else {
                    let queued = queued.context("queued native event")?;
                    assert_eq!(queued.cwd, match case { "missing-start" => "/native/project", "known" => "/known/project", _ => "" });
                    assert_eq!(queued.session_id, "metadata-thread");
                    assert_eq!(queued.turn_id, "metadata-turn");
                    assert_eq!(queued.source, triage::Source::AppServer);
                    assert_eq!(node.ingest_agent_event(turn("metadata-thread", "metadata-turn", "/native/project")).await?["status"], "duplicate");
                    assert_eq!(node.store.lock().await.triage_events(None)?.len(), 1);
                }
                let queries: Vec<_> = std::iter::from_fn(|| queries.try_recv().ok()).collect();
                let reads: Vec<_> = queries.iter().filter(|query| query["method"] == "thread/read").collect();
                if case == "known" {
                    assert!(reads.is_empty());
                } else {
                    assert_eq!(reads.len(), 1);
                    assert_eq!(reads[0]["params"], json!({"threadId":"metadata-thread","includeTurns":false}));
                }
                assert!(!queries.iter().any(|query| query["method"].as_str().is_some_and(|method| method.starts_with("turn/"))));
                Ok::<_, anyhow::Error>(())
            }.await;
            cleanup(node, dir).await?;
            result.with_context(|| case)?;
        }
        Ok(())
    }

    pub(crate) async fn start_reply(
        node: &Runtime,
        admin: &LocalIdentity,
        id: &str,
    ) -> Result<(String, String)> {
        start_reply_in(node, admin, "c", id).await
    }

    async fn start_reply_in(
        node: &Runtime,
        admin: &LocalIdentity,
        scope: &str,
        id: &str,
    ) -> Result<(String, String)> {
        let (trigger, attempt) = claim_reply(node, admin, scope, id, "boot").await?;
        node.store
            .lock()
            .await
            .mark_started(scope, &trigger, &attempt)?;
        Ok((trigger, attempt))
    }

    pub(in crate::runtime) async fn claim_reply(
        node: &Runtime,
        admin: &LocalIdentity,
        scope: &str,
        id: &str,
        boot: &str,
    ) -> Result<(String, String)> {
        let network = node.network_id().await?;
        let posted = admin.attest(
            &network,
            scope,
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
                failure: None,
            },
        )?;
        let signed = posted.attestation().cloned().context("signed")?;
        let mut store = node.store.lock().await;
        store.accept_message(
            &node.identity,
            &network,
            scope,
            id,
            id,
            vec![],
            true,
            signed,
            now_ms(),
        )?;
        let claimed = store
            .claim_next_fixture(&node.identity, &network, scope, boot, "cfg")?
            .context("job")?;
        let trigger = claimed.job.trigger.clone();
        let attempt = claimed.job.state.attempt().context("attempt")?.id.clone();
        store.record_turn_runtime(
            scope,
            &trigger,
            &attempt,
            &crate::scheduling::TurnRuntime {
                provider: Provider::Codex,
                model: Some("fixture".into()),
                reasoning_effort: None,
                generation: node.provider_generation(Provider::Codex, &node.providers())?,
                session_generation: claimed.session_generation,
                delivery_id: attempt.clone(),
                stage: vibeface_bridge::failure::ProviderStage::Validate,
                execution: crate::scheduling::TurnExecution::NotStarted,
                attempted_at_ms: now_ms(),
                switched_from: None,
                switch_reason: None,
            },
        )?;
        Ok((trigger, attempt))
    }

    #[tokio::test]
    async fn a_check_from_another_app_server_stays_pending() -> Result<()> {
        let (node, admin, dir) = node().await?;
        let result = async {
            with_agent(&node);
            bot(&node, &admin, "c", "前端", &[]).await?;
            bot(&node, &admin, "d", "后端", &[]).await?;
            let mut requests =
                scripted_app_server(&node, |request| match request["method"].as_str() {
                    Some("thread/read") => json!({"result":{"thread":{"turns":[
                    {"id":"turn","status":"completed","items":[]}]}}}),
                    _ => json!({"result":{}}),
                })
                .await?;
            tokio::time::timeout(Duration::from_secs(5), connected(&node)).await?;
            let network = node.network_id().await?;
            assert_eq!(node.codex.server(), "system");
            let mut foreign_attempt = String::new();
            for (scope, server) in [("c", "own:"), ("d", "system")] {
                let (trigger, attempt) = start_reply_in(&node, &admin, scope, "m1").await?;
                if server == "own:" {
                    foreign_attempt = attempt.clone();
                }
                let mut store = node.store.lock().await;
                store.record_manager_turn(
                    &attempt,
                    server,
                    "thread",
                    Some("turn"),
                    &Default::default(),
                )?;
                let interrupted = crate::scheduling::JobOutcome::Interrupted {
                    failure: vibeface_bridge::failure::ExecFailure::new(
                        vibeface_bridge::failure::FailureKind::Lost,
                        "lost",
                    ),
                    process_unconfirmed: true,
                };
                store.finish_job(
                    &node.identity,
                    &network,
                    scope,
                    &trigger,
                    &attempt,
                    interrupted,
                    "",
                    now_ms(),
                )?;
            }
            node.reconcile_checks(&network).await?;
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
                "only this App Server's confirmed terminal turn is settled"
            );
            assert_eq!(
                left[0].attempt_id, foreign_attempt,
                "the original foreign attempt stays pending"
            );
            let store = node.store.lock().await;
            let foreign = store
                .scheduler_job("c", "message:m1")?
                .context("foreign job")?;
            assert!(matches!(
                foreign.state,
                crate::scheduling::JobState::Reconciling { .. }
            ));
            assert!(store.message(&network, "c", &foreign.message_id)?.is_none());
            assert!(matches!(
                store
                    .scheduler_job("d", "message:m1")?
                    .context("local job")?
                    .state,
                crate::scheduling::JobState::Completed { .. }
            ));
            let sent: Vec<_> = std::iter::from_fn(|| requests.try_recv().ok()).collect();
            assert_eq!(
                sent.iter()
                    .filter(|request| request["method"] == "thread/read")
                    .count(),
                1
            );
            assert!(
                sent.iter().all(|request| request["method"] != "turn/start"
                    && request["method"] != "turn/steer"),
                "reconciliation never resends either prompt"
            );
            Ok::<_, anyhow::Error>(())
        }
        .await;
        cleanup(node, dir).await?;
        result
    }

    #[tokio::test]
    async fn work_status_lists_dispatches_the_dag_does_not_name_yet() -> Result<()> {
        use crate::work::{Dispatch, DispatchState, WorkSpec};
        let (node, admin, dir) = node().await?;
        let result = async {
            bot(&node, &admin, "c", "前端", &[]).await?;
            let network = node.network_id().await?;
            let trigger = crate::scheduling::message_trigger("m1");
            let spec = crate::work::WorkRequest {
                work_id: WorkSpec::id("c", &trigger, "r"),
                conversation: "c".into(),
                trigger,
                target: LocalIdentity::generate().device_id()?,
                dispatcher: node.identity.device_id()?,
                call_ref: "r".into(),
                kind: crate::work::RequestedWorkKind::Task,
                model: None,
                cwd: "/w".into(),
                title: "t".into(),
                prompt: "p".into(),
                continues: None,
                parent: None,
                thread: None,
            };
            node.store.lock().await.save_dispatch(&Dispatch {
                spec: spec.clone(),
                state: DispatchState::Pending,
            })?;
            let access = Access {
                network_id: network,
                human: false,
                conversation_id: Some("c".into()),
                manager: None,
            };
            let all = node.tool(access.clone(), "work_status", json!({})).await?;
            assert_eq!(
                (&all["works"][0]["work_id"], &all["works"][0]["state"]),
                (&json!(spec.work_id), &json!("dispatch_waiting"))
            );
            let one = node
                .tool(access, "work_status", json!({"work_id": spec.work_id}))
                .await?;
            assert_eq!(one["state"], "dispatch_waiting");
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
            node.register_manager_attempt_fixture(
                "token".into(),
                access.clone(),
                &attempt,
                &trigger,
                crate::providers::Provider::Codex,
            )
            .await?;
            let access = node
                .agent_tokens
                .lock()
                .await
                .get("token")
                .cloned()
                .context("registered tool token")?;
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
    async fn a_work_runs_on_the_provider_it_names() -> Result<()> {
        use crate::providers::Provider;
        let (node, admin, dir) = node().await?;
        let result = async {
            assert!(std::ptr::addr_eq(
                node.executor(Provider::Codex)?,
                &*node.codex
            ));
            assert!(std::ptr::addr_eq(
                node.executor(Provider::Cursor)?,
                &node.cursor_executor
            ));
            with_agent(&node);
            node.settings.lock().unwrap().roots = vec![dir.canonicalize()?];
            let native_cwd = dir.canonicalize()?.display().to_string();
            let mut native_queries = scripted_app_server(&node, move |request| match request["method"].as_str() {
                Some("thread/read") => json!({"result":{"thread":{"id":request["params"]["threadId"],"cwd":native_cwd}}}),
                _ => json!({"result":{}}),
            }).await?;
            tokio::time::timeout(Duration::from_secs(5), connected(&node)).await?;
            bot(&node, &admin, "c", "前端", &[]).await?;
            let network = node.network_id().await?;
            let (trigger, attempt) = start_reply(&node, &admin, "m1").await?;
            let access = Access {
                network_id: network.clone(),
                human: false,
                conversation_id: Some("c".into()),
                manager: None,
            };
            node.register_manager_attempt_fixture("token".into(), access.clone(), &attempt, &trigger, crate::providers::Provider::Codex)
                .await?;
            let access = node.agent_tokens.lock().await.get("token").cloned().context("registered tool token")?;
            let send = |call_ref: &str, agent: Value| {
                json!({"thread":"t-other","call_ref":call_ref,"title":"t","prompt":"p","agent":agent})
            };
            let off = node
                .tool(access.clone(), "track_work", send("r1", json!("claude")))
                .await;
            let named = node
                .tool(access.clone(), "track_work", send("r2", json!("codex")))
                .await?;
            let retried = node
                .tool(access.clone(), "track_work", send("r2", json!("codex")))
                .await?;
            complete_tracked_work(&node, &network, named["work_id"].as_str().context("named work")?).await?;
            let only = node
                .tool(access.clone(), "track_work", send("r3", Value::Null))
                .await?;
            complete_tracked_work(&node, &network, only["work_id"].as_str().context("only work")?).await?;
            {
                let mut providers = node.providers.lock().unwrap();
                providers.switches.execution.insert(Provider::Claude);
                (providers.bins).insert(Provider::Claude, "/nonexistent/claude".into());
            }
            // A Codex thread is no session of Claude Code's own works.
            let foreign = node
                .tool(access.clone(), "track_work", send("r4", json!("claude")))
                .await;
            let defaulted = node
                .tool(access.clone(), "track_work", send("r6", Value::Null))
                .await?;
            complete_tracked_work(&node, &network, defaulted["work_id"].as_str().context("defaulted work")?).await?;
            let local = node.identity.device_id()?;
            let origin = main_input_fact(&node, &admin, "c", "m0").await?;
            let earlier = crate::work::WorkSpec {
                work_id: "w-earlier".into(),
                conversation: "c".into(),
                trigger: origin,
                target: local.clone(),
                dispatcher: local,
                call_ref: "w-earlier".into(),
                kind: crate::model::WorkKind::Agent(Provider::Claude),
                model: None,
                resolved: crate::model::ResolvedModel { provider: Provider::Claude, model: None },
                cwd: String::new(),
                title: "t".into(),
                prompt: "p".into(),
                continues: None,
                parent: None,
                thread: Some("t-claude".into()),
            };
            (node.store.lock().await).accept_work_fixture(&node.identity, &network, earlier, now_ms())?;
            let own = json!({"thread":"t-claude","call_ref":"r5","title":"t","prompt":"p",
                "agent":"claude"});
            let crossed = json!({"thread":"t-claude","call_ref":"r7","title":"t","prompt":"p",
                "agent":"codex"});
            let crossed = node.tool(access.clone(), "track_work", crossed).await;
            let own = node.tool(access, "track_work", own).await;
            node.finish_manager_attempt(&attempt).await;
            let off = off.expect_err("claude executes no work here");
            assert!(format!("{off:#}").contains("turned off"), "{off:#}");
            assert_eq!(named["may_send"], true, "the Codex CLI sends it");
            assert_eq!(retried["may_send"], false, "a retry never sends again");
            assert_eq!(only["may_send"], true, "codex is the only executor");
            let foreign = foreign.expect_err("a session no Claude Code work ran in");
            assert!(
                format!("{foreign:#}").contains("set agent to codex"),
                "{foreign:#}"
            );
            assert_eq!(
                defaulted["may_send"],
                true,
                "with two executors the bot's own provider runs it"
            );
            let on_thread: Vec<_> = node.store.lock().await.works(&network, "c")?.into_iter().filter(|work| {
                work.thread.as_deref() == Some("t-other") || work.thread_id.as_deref() == Some("t-other")
            }).collect();
            assert_eq!(on_thread.len(), 3, "the refused work left nothing behind");
            assert!(
                (on_thread.iter()).all(|run| run.kind == crate::model::WorkKind::Agent(Provider::Codex))
            );
            let crossed = format!("{:#}", crossed.expect_err("Codex into a Claude Code session"));
            assert!(crossed.contains("set agent to claude"), "{crossed}");
            let own = format!("{:#}", own.expect_err("Claude Code refuses the start"));
            assert!(
                own.contains("native session directory could not be verified") && own.contains("invalid character") && own.contains("new call_ref"),
                "the Agent reads why its start was refused: {own}"
            );
            let refused = crate::work::WorkSpec::id("c", &trigger, "r5");
            let refused = (node.store.lock().await.work_record(&refused)?)
                .context("the refused work")?;
            assert_eq!(
                refused.state,
                crate::work::WorkState::Finished {
                    outcome: crate::model::WorkOutcome::Rejected
                }
            );
            let native_queries: Vec<_> = std::iter::from_fn(|| native_queries.try_recv().ok()).collect();
            assert!(native_queries.iter().any(|request| request["method"] == "thread/read"
                && request["params"]["threadId"] == "t-other"), "thread inputs verify the native directory before admission");
            Ok::<_, anyhow::Error>(())
        }
        .await;
        cleanup(node, dir).await?;
        result
    }

    #[tokio::test]
    async fn a_work_asking_what_its_agent_cannot_do_is_refused_before_it_is_registered()
    -> Result<()> {
        use crate::providers::Provider;
        let (node, admin, dir) = node().await?;
        let result = async {
            with_agent(&node);
            node.settings.lock().unwrap().roots = vec![dir.clone()];
            {
                let mut providers = node.providers.lock().unwrap();
                for provider in [Provider::Claude, Provider::Opencode] {
                    providers.switches.execution.insert(provider);
                    let bin = format!("/nonexistent/{}", provider.name());
                    (providers.bins).insert(provider, bin.into());
                }
            }
            bot(&node, &admin, "c", "前端", &[]).await?;
            let network = node.network_id().await?;
            let (trigger, attempt) = start_reply(&node, &admin, "m1").await?;
            let access = Access {
                network_id: network.clone(),
                human: false,
                conversation_id: Some("c".into()),
                manager: None,
            };
            node.register_manager_attempt_fixture(
                "token".into(),
                access.clone(),
                &attempt,
                &trigger,
                Provider::Codex,
            )
            .await?;
            let access = node
                .agent_tokens
                .lock()
                .await
                .get("token")
                .cloned()
                .context("registered tool token")?;
            let asks = |call_ref: &str, agent: &str, extra: Value| {
                let mut args = json!({"cwd":dir.display().to_string(),"call_ref":call_ref,
                    "title":"t","prompt":"p","agent":agent});
                args.as_object_mut()
                    .unwrap()
                    .extend(extra.as_object().unwrap().clone());
                args
            };
            let images = node
                .tool(
                    access.clone(),
                    "track_work",
                    asks("r1", "claude", json!({"images":["/a.png"]})),
                )
                .await;
            let effort = node
                .tool(
                    access.clone(),
                    "track_work",
                    asks("r2", "opencode", json!({"effort":"high"})),
                )
                .await;
            let plain = node
                .tool(
                    access.clone(),
                    "track_work",
                    asks("r3", "claude", json!({"images":[]})),
                )
                .await;
            node.finish_manager_attempt(&attempt).await;
            let images = format!("{:#}", images.expect_err("Claude Code takes no images"));
            assert!(
                images.contains("takes no images") && images.contains("\"pi\""),
                "{images}"
            );
            let effort = format!("{:#}", effort.expect_err("opencode takes no effort"));
            assert!(effort.contains("takes no effort"), "{effort}");
            let plain = format!(
                "{:#}",
                plain.expect_err("the start fails on the missing program")
            );
            assert!(
                !plain.contains("takes no"),
                "an empty image list asks for nothing: {plain}"
            );
            for call_ref in ["r1", "r2"] {
                let work_id = crate::work::WorkSpec::id("c", &trigger, call_ref);
                assert!(
                    node.store.lock().await.work_record(&work_id)?.is_none(),
                    "{call_ref} left no work behind"
                );
            }
            Ok::<_, anyhow::Error>(())
        }
        .await;
        cleanup(node, dir).await?;
        result
    }

    #[tokio::test]
    async fn a_work_without_agent_goes_on_where_its_thread_or_original_ran() -> Result<()> {
        use crate::model::WorkKind;
        use crate::providers::Provider;
        let (node, admin, dir) = node().await?;
        let result = async {
            with_agent(&node);
            if let Some(manager) = node.settings.lock().unwrap().manager.as_mut() {
                manager.provider = Provider::Claude;
            }
            node.settings.lock().unwrap().roots = vec![dir.clone()];
            {
                let mut providers = node.providers.lock().unwrap();
                providers.switches.execution.insert(Provider::Claude);
                (providers.bins).insert(Provider::Claude, "/nonexistent/claude".into());
            }
            node.settings.lock().unwrap().roots = vec![dir.canonicalize()?];
            let native_cwd = dir.canonicalize()?.display().to_string();
            let mut native_queries = scripted_app_server(&node, move |request| match request["method"].as_str() {
                Some("thread/read") => json!({"result":{"thread":{"id":request["params"]["threadId"],"cwd":native_cwd}}}),
                _ => json!({"result":{}}),
            }).await?;
            tokio::time::timeout(Duration::from_secs(5), connected(&node)).await?;
            bot(&node, &admin, "c", "前端", &[]).await?;
            let network = node.network_id().await?;
            let (trigger, attempt) = start_reply(&node, &admin, "m1").await?;
            let access = Access {
                network_id: network.clone(),
                human: false,
                conversation_id: Some("c".into()),
                manager: None,
            };
            node.register_manager_attempt_fixture(
                "token".into(),
                access.clone(),
                &attempt,
                &trigger,
                Provider::Claude,
            )
            .await?;
            let access = node.agent_tokens.lock().await.get("token").cloned().context("registered tool token")?;
            let local = node.identity.device_id()?;
            let origin = main_input_fact(&node, &admin, "c", "m0").await?;
            let original = crate::work::WorkSpec {
                work_id: "w-codex".into(),
                conversation: "c".into(),
                trigger: origin,
                target: local.clone(),
                dispatcher: local,
                call_ref: "w-codex".into(),
                kind: WorkKind::Agent(Provider::Codex),
                model: None,
                resolved: crate::model::ResolvedModel { provider: Provider::Codex, model: None },
                cwd: String::new(),
                title: "t".into(),
                prompt: "p".into(),
                continues: None,
                parent: None,
                thread: Some("t-done".into()),
            };
            (node.store.lock().await).accept_work_fixture(&node.identity, &network, original, now_ms())?;
            complete_tracked_work(&node, &network, "w-codex").await?;
            let cwd = dir.display().to_string();
            let call = |args: Value| node.tool(access.clone(), "track_work", args);
            let answered = call(json!({"thread":"t-codex","call_ref":"a1","title":"t","prompt":"yes"})).await?;
            complete_tracked_work(&node, &network, answered["work_id"].as_str().context("answered work")?).await?;
            let followed = call(json!({"continues":"w-codex","cwd":cwd,"call_ref":"f1","title":"t","prompt":"more"})).await?;
            node.bind_work(
                &network,
                "c",
                followed["work_id"].as_str().context("followed work")?,
                vibeface_bridge::codex::StartReport::Thread {
                    thread_id: followed["thread_id"].as_str().context("continued thread")?.into(),
                },
            )
            .await?;
            complete_tracked_work(&node, &network, followed["work_id"].as_str().context("followed work")?).await?;
            let crossed = call(json!({"continues":"w-codex","cwd":cwd,"call_ref":"f2","title":"t","prompt":"more","agent":"claude"})).await;
            let foreign = call(json!({"thread":"t-codex","call_ref":"a2","title":"t","prompt":"yes","agent":"claude"})).await;
            let ready = node.tool(access.clone(), "wait_work", json!({"thread":"t-codex"})).await;
            {
                let mut providers = node.providers.lock().unwrap();
                providers.switches.execution = [Provider::Codex].into();
            }
            let fresh = call(json!({"cwd":cwd,"call_ref":"n1","title":"t","prompt":"new"})).await;
            node.providers.lock().unwrap().switches.execution.insert(Provider::Claude);
            let retried = call(json!({"cwd":cwd,"call_ref":"n1","title":"t","prompt":"new"})).await;
            let changed = call(json!({"cwd":cwd,"call_ref":"n1","title":"t","prompt":"other"})).await;
            node.finish_manager_attempt(&attempt).await;

            let kind = |work_id: &Value| -> Result<WorkKind> {
                let id = work_id.as_str().context("work_id")?;
                Ok((node.store.try_lock()?.work_record(id)?).context("recorded")?.kind)
            };
            assert_eq!(answered["may_send"], true, "the Codex CLI sends the answer");
            assert_eq!(kind(&answered["work_id"])?, WorkKind::Agent(Provider::Codex));
            assert_eq!(kind(&followed["work_id"])?, WorkKind::Agent(Provider::Codex));
            for (refused, what) in [(crossed, "continuation"), (foreign, "thread")] {
                let refused = format!("{:#}", refused.expect_err(what));
                assert!(refused.contains("agent") && refused.contains("Codex"), "{what}: {refused}");
            }
            assert_eq!(ready?["agent"], "codex", "the CLI reads a Codex thread first");
            let fresh = fresh?;
            assert_eq!(kind(&fresh["work_id"])?, WorkKind::Agent(Provider::Codex));
            let retried = retried?;
            assert_eq!(retried["work_id"], fresh["work_id"]);
            assert_eq!(retried["may_send"], false, "a retry only reads back");
            let changed = format!("{:#}", changed.expect_err("another prompt under one call_ref"));
            assert!(changed.contains("prompt") && !changed.contains("kind"), "{changed}");
            let native_queries: Vec<_> = std::iter::from_fn(|| native_queries.try_recv().ok()).collect();
            assert!(native_queries.iter().any(|request| request["method"] == "thread/read"
                && request["params"]["threadId"] == "t-codex"), "thread inputs verify the native directory before admission");
            Ok::<_, anyhow::Error>(())
        }
        .await;
        cleanup(node, dir).await?;
        result
    }

    #[tokio::test]
    async fn a_bad_call_ref_is_named_so_the_agent_fixes_it_at_once() -> Result<()> {
        let (node, admin, dir) = node().await?;
        let result = async {
            with_agent(&node);
            bot(&node, &admin, "c", "前端", &[]).await?;
            let network = node.network_id().await?;
            let (trigger, attempt) = start_reply(&node, &admin, "m1").await?;
            let access = Access {
                network_id: network.clone(),
                human: false,
                conversation_id: Some("c".into()),
                manager: None,
            };
            node.register_manager_attempt_fixture(
                "token".into(),
                access.clone(),
                &attempt,
                &trigger,
                crate::providers::Provider::Codex,
            )
            .await?;
            let access = node
                .agent_tokens
                .lock()
                .await
                .get("token")
                .cloned()
                .context("registered tool token")?;
            let create = |call_ref: &str| json!({"call_ref":call_ref,"name":"2SOMEone/LEAPERone"});
            let refused = node
                .tool(access.clone(), "create_bot", create("bots/2SOMEone"))
                .await;
            let created = node
                .tool(access, "create_bot", create("create-2someone"))
                .await;
            node.finish_manager_attempt(&attempt).await;
            let refused = refused.expect_err("a call_ref with a slash");
            assert!(
                refused
                    .to_string()
                    .starts_with("arguments.call_ref is invalid: id must contain"),
                "{refused:#}"
            );
            assert_eq!(created?["name"], "2SOMEone/LEAPERone");
            Ok::<_, anyhow::Error>(())
        }
        .await;
        cleanup(node, dir).await?;
        result
    }

    #[derive(Clone, Copy)]
    enum CoordinationReply {
        Complete,
        Refused,
        Failed,
        Unknown,
    }

    async fn coordination_app_server(
        node: &Runtime,
    ) -> Result<tokio::sync::mpsc::UnboundedReceiver<Value>> {
        coordination_server(node, CoordinationReply::Complete).await
    }

    async fn coordination_server(
        node: &Runtime,
        response: CoordinationReply,
    ) -> Result<tokio::sync::mpsc::UnboundedReceiver<Value>> {
        use futures_util::{SinkExt as _, StreamExt as _};
        let mut listener = vibeface_bridge::platform::ipc::Listener::bind(
            &vibeface_bridge::platform::ipc::Endpoint::from_path(&named_socket(node))?,
        )
        .await?;
        let (sent, received) = tokio::sync::mpsc::unbounded_channel();
        tokio::spawn(async move {
            loop {
                let stream = tokio::select! {
                    _ = sent.closed() => break,
                    accepted = listener.accept() => match accepted { Ok(stream) => stream, Err(_) => break },
                };
                let sent = sent.clone();
                tokio::spawn(async move {
                    let Ok(mut server) = tokio_tungstenite::accept_async(stream).await else {
                        return;
                    };
                    loop {
                        let message = tokio::select! {
                            _ = sent.closed() => break,
                            message = server.next() => match message { Some(Ok(message)) => message, _ => break },
                        };
                        let Ok(request) = serde_json::from_slice::<Value>(&message.into_data())
                        else {
                            continue;
                        };
                        if request.get("id").is_none() {
                            continue;
                        }
                        let _ = sent.send(request.clone());
                        if request["method"] == "turn/start" {
                            match response {
                                CoordinationReply::Unknown => break,
                                CoordinationReply::Refused => {
                                    let reply = json!({"id":request["id"],"error":{"code":-32602,"message":"fixture refused before model start"}});
                                    let _ = server.send(reply.to_string().into()).await;
                                    continue;
                                }
                                _ => {}
                            }
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
                                "turn":{"id":"coord-turn","status":if matches!(response, CoordinationReply::Failed) { "failed" } else { "completed" }}}}));
                        }
                        for reply in replies {
                            let _ = server.send(reply.to_string().into()).await;
                        }
                    }
                });
            }
        });
        tokio::time::timeout(Duration::from_secs(5), connected(node)).await?;
        Ok(received)
    }

    fn turn_starts(requests: &mut tokio::sync::mpsc::UnboundedReceiver<Value>) -> Vec<String> {
        std::iter::from_fn(|| requests.try_recv().ok())
            .filter(|request| request["method"] == "turn/start")
            .map(|request| request["params"].to_string())
            .collect()
    }

    #[tokio::test]
    async fn the_coordination_agent_sees_the_owning_bots_instructions_and_notes() -> Result<()> {
        let (node, admin, dir) = node().await?;
        let result = async {
            with_agent(&node);
            let instructions = format!(
                "只管前端仓库，发版前先问用户。{}截断之后",
                "很".repeat(3000)
            );
            instructed_bot(&node, &admin, "c", "前端", &instructions, &[]).await?;
            let network = node.network_id().await?;
            for (index, text) in ["用户每周五才发版".to_owned(), "长".repeat(500)]
                .into_iter()
                .enumerate()
            {
                let body = admin.attest(
                    &network,
                    "c",
                    EventBody::MemoryControlled {
                        memory_id: format!("memory-{index}"),
                        base: crate::model::MemoryBase::absent(),
                        after: Some(text),
                        attestation: None,
                    },
                )?;
                node.store.lock().await.append_event(
                    &node.identity,
                    &network,
                    "c",
                    body,
                    vec![],
                    now_ms() + index as i64,
                )?;
            }
            let mut requests = coordination_app_server(&node).await?;
            node.ingest_agent_event(turn("s1", "t1", "/tmp")).await?;
            node.wake_agent().await?;
            let prompt = turn_starts(&mut requests).concat();
            assert!(
                prompt.contains("只管前端仓库，发版前先问用户。"),
                "the bot's instructions"
            );
            assert!(
                prompt.contains("用户每周五才发版"),
                "older notes stay when the newest is too long"
            );
            assert!(
                !prompt.contains("长长长"),
                "a note too long to fit is left out"
            );
            assert!(
                !prompt.contains("截断之后"),
                "only the start of long instructions"
            );
            Ok::<_, anyhow::Error>(())
        }
        .await;
        cleanup(node, dir).await?;
        result
    }

    #[tokio::test]
    async fn coordination_budget_keeps_unoffered_events_and_request_deadlines() -> Result<()> {
        let (node, admin, dir) = node().await?;
        let result = async {
            with_agent(&node);
            bot(&node, &admin, "c", "前端", &[]).await?;
            let network = node.network_id().await?;
            let limit = node.config.daily_decision_limit;
            let triage = format!("triage:{network}");
            node.store
                .lock()
                .await
                .reserve_budget(&triage, limit - 1, limit)?;
            let mut requests = coordination_app_server(&node).await?;

            node.ingest_agent_event(turn("s1", "t1", "/tmp")).await?;
            node.wake_agent().await?;
            assert_eq!(
                turn_starts(&mut requests).len(),
                1,
                "the last turn of the day runs"
            );

            let mut deferred = turn("s-deferred", "t-deferred", "/tmp");
            deferred.event_id = AgentEvent::id_for(
                &node.identity.device_id()?,
                deferred.agent,
                &deferred.session_id,
                &deferred.turn_id,
                deferred.event,
            );
            let deferred_id = deferred.event_id.clone();
            node.ingest_agent_event(deferred).await?;
            let held = node.ingest_agent_event(asks_permission("s2")).await?;
            let request_id = held["request_id"]
                .as_str()
                .context("a held request")?
                .to_owned();
            node.wake_agent().await?;
            assert_eq!(
                node.triage_status().await?["budget"]["paused_reason"],
                "budget_exhausted"
            );
            assert!(
                turn_starts(&mut requests).is_empty(),
                "no turn past the budget"
            );
            assert!(
                node.triage
                    .requests
                    .lock()
                    .unwrap()
                    .contains_key(&request_id),
                "a budget refusal must not shorten the request deadline"
            );
            node.store
                .lock()
                .await
                .reserve_budget(&network, limit, limit)
                .context("the bots' own turns keep their whole budget")?;
            let pending = node.pending_events().await?;
            assert_eq!(pending.len(), 2, "events were not offered to the model");
            assert!(pending.iter().any(|event| event.event_id == deferred_id));
            let deadline = lock(&node.triage.requests)[&request_id].answer_by_ms;
            node.wake_agent().await?;
            assert_eq!(
                lock(&node.triage.requests)[&request_id].answer_by_ms,
                deadline
            );
            assert_eq!(
                lock(&node.triage.requests)[&request_id].fallback_reason,
                Some("budget_exhausted")
            );
            lock(&node.triage.requests)
                .get_mut(&request_id)
                .unwrap()
                .answer_by_ms = Some(now_ms() - 1);
            node.expire_requests().await?;
            node.expire_requests().await?;
            assert_eq!(
                lock(&node.triage.answered).len(),
                1,
                "deadline fallback happens once"
            );
            let retained = node.pending_events().await?;
            assert_eq!(
                retained.len(),
                2,
                "an expired request still leaves its event for review"
            );
            assert!(
                retained
                    .iter()
                    .all(|event| event.request_id.is_none() && event.hold.is_none())
            );
            node.store.lock().await.connection.execute(
                "UPDATE budget_days SET used=0 WHERE network_id=? AND day=date('now')",
                [&triage],
            )?;
            node.wake_agent().await?;
            node.wake_agent().await?;
            assert_eq!(
                turn_starts(&mut requests).len(),
                1,
                "budget recovery reviews the saved batch once"
            );
            assert!(node.pending_events().await?.is_empty());
            assert_eq!(
                node.ingest_agent_event(turn("s-deferred", "t-deferred", "/tmp"))
                    .await?["status"],
                "duplicate"
            );
            Ok::<_, anyhow::Error>(())
        }
        .await;
        cleanup(node, dir).await?;
        result
    }

    #[tokio::test]
    async fn triage_refunds_only_live_proven_unstarted_reservations_on_the_original_day()
    -> Result<()> {
        let (node, admin, dir) = node().await?;
        let result = async {
            with_agent(&node);
            bot(&node, &admin, "c", "前端", &[]).await?;
            node.ingest_agent_event(turn("first", "one", "/tmp"))
                .await?;
            node.ingest_agent_event(turn("second", "two", "/tmp"))
                .await?;
            let network = node.network_id().await?;
            let original = now_ms() - 86_400_000;
            let refused = Err(manager::ManagerError::not_started(anyhow::anyhow!(
                "fixture before start"
            )));
            let mut store = node.store.lock().await;
            let TriageAdmission::Claimed(attempt) =
                store.admit_triage(&network, &node.boot, 1, 1, original)?
            else {
                bail!("first admission");
            };
            let event = attempt.events()[0].clone();
            assert_eq!(
                store
                    .triage_status(&network, &node.boot, Some(attempt.id()), 1, original)?
                    .budget
                    .used,
                1
            );
            store.reserve_budget(&format!("triage:{network}"), 7, 100)?;
            dequeue(&store.connection, &event)?;
            store.finish_triage(attempt, &refused)?;
            assert_eq!(
                store
                    .triage_status(&network, &node.boot, None, 1, original)?
                    .budget
                    .used,
                0,
                "refund the original UTC day"
            );
            assert_eq!(
                store
                    .triage_status(&network, &node.boot, None, 100, now_ms())?
                    .budget
                    .used,
                7,
                "today's unrelated charge stays"
            );
            let pending = store.triage_events(None)?;
            assert_eq!(
                pending.len(),
                1,
                "release does not resurrect a removed event"
            );
            assert_ne!(pending[0].event_id, event.event_id);
            let TriageAdmission::Claimed(attempt) =
                store.admit_triage(&network, &node.boot, 1, 100, now_ms())?
            else {
                bail!("same-day admission");
            };
            store.finish_triage(attempt, &refused)?;
            assert_eq!(
                store
                    .triage_status(&network, &node.boot, None, 100, now_ms())?
                    .budget
                    .used,
                7,
                "same-day release refunds exactly one reservation"
            );
            Ok::<_, anyhow::Error>(())
        }
        .await;
        cleanup(node, dir).await?;
        result
    }

    #[tokio::test]
    async fn concurrent_triage_admissions_keep_the_daily_cap() -> Result<()> {
        let (node, admin, dir) = node().await?;
        let result = async {
            with_agent(&node);
            bot(&node, &admin, "c", "前端", &[]).await?;
            for index in 0..4 {
                node.ingest_agent_event(turn(&format!("session-{index}"), "turn", "/tmp"))
                    .await?;
            }
            let network = node.network_id().await?;
            let barrier = Arc::new(std::sync::Barrier::new(4));
            let mut tasks = Vec::new();
            for _ in 0..4 {
                let mut store = crate::store::Store::open(dir.join("node.sqlite3"))?;
                let network = network.clone();
                let boot = node.boot.clone();
                let barrier = barrier.clone();
                tasks.push(std::thread::spawn(move || {
                    barrier.wait();
                    store.admit_triage(&network, &boot, 1, 1, now_ms())
                }));
            }
            let mut claimed = 0;
            let mut paused = 0;
            let mut reservation = None;
            for task in tasks {
                match task.join().expect("concurrent SQLite admission")? {
                    TriageAdmission::Claimed(attempt) => {
                        claimed += 1;
                        reservation = Some(attempt);
                    }
                    TriageAdmission::Paused(_) => paused += 1,
                    TriageAdmission::Empty => bail!("unclaimed events remain"),
                }
            }
            assert_eq!((claimed, paused), (1, 3));
            assert_eq!(node.triage_status().await?["budget"]["used"], 1);
            assert_eq!(node.pending_events().await?.len(), 3);
            let mut release_store = crate::store::Store::open(dir.join("node.sqlite3"))?;
            let mut reserve_store = crate::store::Store::open(dir.join("node.sqlite3"))?;
            let barrier = Arc::new(std::sync::Barrier::new(2));
            let release_barrier = barrier.clone();
            let release = std::thread::spawn(move || {
                release_barrier.wait();
                release_store.finish_triage(
                    reservation.expect("one reservation"),
                    &Err(manager::ManagerError::not_started(anyhow::anyhow!(
                        "not sent"
                    ))),
                )
            });
            barrier.wait();
            let admission = reserve_store.admit_triage(&network, &node.boot, 1, 1, now_ms())?;
            release.join().expect("concurrent SQLite release")?;
            let expected_used = u64::from(matches!(admission, TriageAdmission::Claimed(_)));
            assert_eq!(
                node.triage_status().await?["budget"]["used"],
                expected_used,
                "concurrent release and admission conserve exactly their own charge"
            );
            assert_eq!(
                node.pending_events().await?.len(),
                4 - expected_used as usize
            );
            Ok::<_, anyhow::Error>(())
        }
        .await;
        cleanup(node, dir).await?;
        result
    }

    #[tokio::test]
    async fn triage_local_setup_failures_release_their_reservation_without_handing_over_events()
    -> Result<()> {
        let (node, admin, dir) = node().await?;
        let result = async {
            with_agent(&node);
            bot(&node, &admin, "c", "前端", &[]).await?;
            let mut calls = coordination_app_server(&node).await?;
            node.settings
                .lock()
                .unwrap()
                .manager
                .as_mut()
                .unwrap()
                .workspace = dir.join("absent-workspace");
            node.ingest_agent_event(turn("local-failure", "t", "/tmp"))
                .await?;
            let error = node
                .wake_agent()
                .await
                .expect_err("invalid manager workspace");
            assert!(error.to_string().contains("manager workspace"), "{error:#}");
            assert_eq!(
                error
                    .downcast_ref::<manager::ManagerError>()
                    .context("setup evidence")?
                    .start,
                manager::StartEvidence::NotStarted
            );
            let status = node.triage_status().await?;
            assert_eq!(status["budget"]["used"], 0);
            assert_eq!(status["pending_events"], 1);
            assert_eq!(status["unknown_events"], 0);
            assert!(turn_starts(&mut calls).is_empty());
            Ok::<_, anyhow::Error>(())
        }
        .await;
        cleanup(node, dir).await?;
        result
    }

    #[tokio::test]
    async fn triage_start_evidence_controls_refunds_and_automatic_retries() -> Result<()> {
        for (reply, expected_start, charged, pending) in [
            (
                CoordinationReply::Refused,
                manager::StartEvidence::NotStarted,
                0,
                1,
            ),
            (
                CoordinationReply::Failed,
                manager::StartEvidence::Started,
                1,
                0,
            ),
            (
                CoordinationReply::Unknown,
                manager::StartEvidence::Unknown,
                1,
                0,
            ),
        ] {
            let (node, admin, dir) = node().await?;
            let result = async {
                with_agent(&node);
                bot(&node, &admin, "c", "前端", &[]).await?;
                let mut calls = coordination_server(&node, reply).await?;
                node.ingest_agent_event(turn("s", "t", "/tmp")).await?;
                let error = node
                    .wake_agent()
                    .await
                    .expect_err("controlled provider failure");
                assert_eq!(
                    error
                        .downcast_ref::<manager::ManagerError>()
                        .context("manager start evidence")?
                        .start,
                    expected_start
                );
                let status = node.triage_status().await?;
                assert_eq!(status["budget"]["used"], charged);
                assert_eq!(status["pending_events"], pending);
                assert_eq!(status["unknown_events"], 1 - pending);
                assert_eq!(turn_starts(&mut calls).len(), 1);
                let retry = node.wake_agent().await;
                if pending == 1 {
                    assert!(retry.is_err(), "only proven not-started rows are retryable");
                    assert_eq!(turn_starts(&mut calls).len(), 1);
                    assert_eq!(node.triage_status().await?["budget"]["used"], 0);
                } else {
                    retry?;
                    assert!(
                        turn_starts(&mut calls).is_empty(),
                        "started or unknown is never replayed"
                    );
                    assert_eq!(
                        node.triage_command(json!({"action":"events"})).await?["unknown_events"]
                            .as_array()
                            .unwrap()
                            .len(),
                        1
                    );
                }
                Ok::<_, anyhow::Error>(())
            }
            .await;
            cleanup(node, dir).await?;
            result?;
        }
        Ok(())
    }

    #[tokio::test]
    async fn triage_claims_gate_late_mutators_and_keep_handled_results_read_only() -> Result<()> {
        let (node, admin, dir) = node().await?;
        let result = async {
            with_agent(&node);
            bot(&node, &admin, "c", "前端", &[]).await?;
            let queued = node.ingest_agent_event(turn("ordinary", "t", "/tmp")).await?;
            let held = node.ingest_agent_event(asks_permission("request")).await?;
            let network = node.network_id().await?;
            let TriageAdmission::Claimed(attempt) = node.store.lock().await.admit_triage(&network, &node.boot, BATCH, 100, now_ms())? else { bail!("admission"); };
            let id = attempt.id().to_owned();
            *lock(&node.triage.active) = Some(id.clone());
            let raise = json!({"action":"raise","event_id":queued["event_id"],"severity":"attention","question":"继续吗？"});
            let respond = json!({"action":"respond","request_id":held["request_id"],"result":{"behavior":"allow"}});
            for action in [&raise, &respond, &json!({"action":"dismiss","event_id":queued["event_id"]})] {
                assert!(node.triage_command(action.clone()).await.is_err(), "an unscoped caller cannot change a claimed row");
                let mut late = action.clone(); late["_attempt"] = json!("old-attempt");
                assert!(node.triage_command(late).await.is_err(), "an older attempt cannot mutate the current one");
            }
            assert!(unanswered(&node).await?.is_empty());
            assert!(lock(&node.triage.answered).is_empty());
            assert_eq!(node.triage_command(json!({"action":"events","_attempt":id})).await?["events"].as_array().unwrap().len(), 2);
            let mut current = raise.clone(); current["_attempt"] = json!(id);
            let raised = node.triage_command(current.clone()).await?;
            let mut current_response = respond.clone(); current_response["_attempt"] = json!(id);
            assert_eq!(node.triage_command(current_response).await?["answered"], true);
            let retained = node.store.lock().await.triage_events(Some((&node.boot, &id)))?;
            assert_eq!(retained.len(), 1, "respond keeps its claimed event until the turn is settled");
            assert_eq!(retained[0].event_id, held["event_id"].as_str().unwrap());
            assert!(retained[0].request_id.is_none() && retained[0].hold.is_none());
            assert_eq!(unanswered(&node).await?.len(), 1, "the active claim can ask its question");
            *lock(&node.triage.active) = None;
            assert_eq!(node.triage_command(current).await?, raised, "a late repeated action only reads its original result");
            assert_eq!(unanswered(&node).await?.len(), 1);
            node.store.lock().await.finish_triage(attempt, &Ok(()))?;
            assert!(node.store.lock().await.unknown_triage_events(None)?.is_empty(), "a complete turn settles the retained response event");

            let failed_response = node.ingest_agent_event(asks_permission("respond-before-failure")).await?;
            let TriageAdmission::Claimed(attempt) = node.store.lock().await.admit_triage(&network, &node.boot, BATCH, 100, now_ms())? else { bail!("respond before failure claim"); };
            let failed_id = attempt.id().to_owned();
            *lock(&node.triage.active) = Some(failed_id.clone());
            let response = json!({"action":"respond","_attempt":failed_id,"request_id":failed_response["request_id"],"result":{"behavior":"allow"}});
            let answered = node.triage_command(response.clone()).await?;
            assert_eq!(answered["answered"], true);
            let failure = Err(manager::ManagerError {
                stage: vibeface_bridge::failure::ProviderStage::Run,
                outcome: manager::ManagerFailure::Failed,
                start: manager::StartEvidence::Started,
                session_id: Some("fixture-started-turn".into()),
                failure: vibeface_bridge::failure::ExecFailure::new(vibeface_bridge::failure::FailureKind::Rejected, "fixture failed after responding"),
            }.into());
            node.store.lock().await.finish_triage(attempt, &failure)?;
            *lock(&node.triage.active) = None;
            let unknown_response = node.store.lock().await.unknown_triage_events(None)?;
            assert_eq!(unknown_response.len(), 1, "a failed turn retains the already-sent response's claim");
            assert_eq!(unknown_response[0].event_id, failed_response["event_id"].as_str().unwrap());
            assert!(unknown_response[0].request_id.is_none() && unknown_response[0].hold.is_none());
            assert_eq!(node.triage_command(response).await?, answered, "the same process only reads its first response result");
            assert_eq!(node.triage_status().await?["budget"]["used"], 2, "a model failure after responding keeps its charge");

            let unknown = node.ingest_agent_event(turn("uncertain", "t", "/tmp")).await?;
            let held = node.ingest_agent_event(asks_permission("uncertain-request")).await?;
            let TriageAdmission::Claimed(attempt) = node.store.lock().await.admit_triage(&network, &node.boot, BATCH, 100, now_ms())? else { bail!("unknown admission"); };
            let stale = attempt.id().to_owned();
            *lock(&node.triage.active) = Some(stale.clone());
            let failure = Err(anyhow::anyhow!("unknown provider result"));
            node.store.lock().await.finish_triage(attempt, &failure)?;
            for action in [
                json!({"action":"dismiss","event_id":unknown["event_id"],"_attempt":stale}),
                json!({"action":"raise","event_id":unknown["event_id"],"_attempt":stale,"severity":"attention","question":"不应提问"}),
                json!({"action":"respond","request_id":held["request_id"],"_attempt":stale,"result":{"behavior":"allow"}}),
            ] { assert!(node.triage_command(action).await.is_err(), "Unknown rows are fenced even within the same boot"); }
            assert_eq!(unanswered(&node).await?.len(), 1);
            assert_eq!(lock(&node.triage.answered).len(), 2);
            *lock(&node.triage.active) = None;
            Ok::<_, anyhow::Error>(())
        }.await;
        cleanup(node, dir).await?;
        result
    }

    #[tokio::test]
    async fn restarting_triage_fences_every_claim_and_still_reviews_new_pending_events()
    -> Result<()> {
        let (node, admin, dir) = node().await?;
        let before = async {
            with_agent(&node);
            bot(&node, &admin, "c", "前端", &[]).await?;
            let network = node.network_id().await?;
            let mut calls = coordination_app_server(&node).await?;
            let answered_request = node.ingest_agent_event(asks_permission("respond-before-crash")).await?;
            let TriageAdmission::Claimed(answered_attempt) = node.store.lock().await.admit_triage(&network, &node.boot, 1, 100, now_ms())? else { bail!("response claim"); };
            *lock(&node.triage.active) = Some(answered_attempt.id().into());
            assert_eq!(node.triage_command(json!({"action":"respond","_attempt":answered_attempt.id(),"request_id":answered_request["request_id"],"result":{"behavior":"allow"}})).await?["answered"], true);
            let retained = node.store.lock().await.triage_events(Some((&node.boot, answered_attempt.id())))?;
            assert_eq!(retained.len(), 1, "a sent response keeps its durable claim before finish");
            assert!(retained[0].request_id.is_none());
            drop(answered_attempt);
            *lock(&node.triage.active) = None;
            for session in ["before-send", "accepted", "action", "unreviewed", "pending"] {
                node.ingest_agent_event(turn(session, "t", "/tmp")).await?;
            }
            let TriageAdmission::Claimed(before_send) =
                node.store
                    .lock()
                    .await
                    .admit_triage(&network, &node.boot, 1, 100, now_ms())?
            else {
                bail!("before-send claim");
            };
            drop(before_send);
            let TriageAdmission::Claimed(accepted) =
                node.store
                    .lock()
                    .await
                    .admit_triage(&network, &node.boot, 1, 100, now_ms())?
            else {
                bail!("accepted claim");
            };
            node.coordination_turn(Some(accepted.id()), |_| {
                Ok("controlled accepted prompt".into())
            })
            .await?;
            assert_eq!(turn_starts(&mut calls).len(), 1);
            drop(accepted);
            let TriageAdmission::Claimed(after_action) =
                node.store
                    .lock()
                    .await
                    .admit_triage(&network, &node.boot, 2, 100, now_ms())?
            else {
                bail!("action claim");
            };
            *lock(&node.triage.active) = Some(after_action.id().into());
            let event = after_action.events()[0].event_id.clone();
            let action = json!({"action":"dismiss","event_id":event,"_attempt":after_action.id()});
            let dismissed = node.triage_command(action.clone()).await?;
            drop(after_action);
            Ok::<_, anyhow::Error>((action, dismissed, answered_request))
        }
        .await;
        let endpoint = vibeface_bridge::platform::ipc::Endpoint::from_path(&named_socket(&node))?;
        node.shutdown().await;
        drop(node);
        tokio::time::timeout(Duration::from_secs(5), async {
            while endpoint.is_held() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await?;
        let reopened = Arc::new(Runtime::reopen(&dir).await?);
        let result = async {
            let (old_action, dismissed, answered_request) = before?;
            with_agent(&reopened);
            let mut calls = coordination_app_server(&reopened).await?;
            let status = reopened.triage_status().await?;
            assert_eq!(status["budget"]["used"], 4, "no crash restores refund authority");
            assert_eq!(status["unknown_events"], 4, "reserve, accepted, response and action-before-finish crashes remain observable");
            assert_eq!(status["pending_events"], 1);
            assert_eq!(reopened.triage_command(old_action).await?, dismissed, "a completed action is only read back");
            let unknown = reopened.triage_command(json!({"action":"events"})).await?["unknown_events"].as_array().unwrap().clone();
            assert!(unknown.iter().any(|event| event["event_id"] == answered_request["event_id"] && event["request_id"].is_null()), "reopening retains the response claim without a reply transport");
            assert!(lock(&reopened.triage.answered).is_empty());
            assert!(reopened.triage_command(json!({"action":"respond","request_id":answered_request["request_id"],"result":{"behavior":"allow"}})).await.is_err(), "a restart does not resend the old response");
            for event in &unknown {
                for action in [
                    json!({"action":"dismiss","event_id":event["event_id"]}),
                    json!({"action":"raise","event_id":event["event_id"],"severity":"attention","question":"迟到"}),
                ] { assert!(reopened.triage_command(action).await.is_err(), "an older boot cannot mutate its unknown claim"); }
            }
            reopened.wake_agent().await?;
            reopened.wake_agent().await?;
            assert_eq!(turn_starts(&mut calls).len(), 1, "only the positive Pending control starts");
            assert_eq!(reopened.triage_status().await?["budget"]["used"], 5);
            assert_eq!(reopened.triage_status().await?["unknown_events"], 4);
            assert!(unanswered(&reopened).await?.is_empty(), "no previous action was replayed");
            Ok::<_, anyhow::Error>(())
        }.await;
        cleanup(reopened, dir).await?;
        result
    }

    #[tokio::test]
    async fn restarting_triage_retains_orphan_events_without_recreating_request_transports()
    -> Result<()> {
        let (node, admin, dir) = node().await?;
        let before = async {
            with_agent(&node);
            bot(&node, &admin, "c", "前端", &[]).await?;
            let held = node.ingest_agent_event(asks_permission("orphan")).await?;
            node.ingest_agent_event(turn("ordinary", "t", "/tmp"))
                .await?;
            Ok::<_, anyhow::Error>(held["request_id"].as_str().context("request")?.to_owned())
        }
        .await;
        node.shutdown().await;
        drop(node);
        let reopened = Arc::new(Runtime::reopen(&dir).await?);
        let result = async {
            let request = before?;
            with_agent(&reopened);
            let pending = reopened.pending_events().await?;
            assert_eq!(pending.len(), 2);
            assert!(pending.iter().all(|event| event.request_id.is_none() && event.hold.is_none()));
            assert!(lock(&reopened.triage.requests).is_empty());
            assert!(lock(&reopened.triage.answered).is_empty());
            assert!(reopened.triage_command(json!({"action":"respond","request_id":request,"result":{"behavior":"allow"}})).await.is_err());
            assert_eq!(reopened.wait_request(&request).await, undecided());
            let mut calls = coordination_app_server(&reopened).await?;
            reopened.wake_agent().await?;
            reopened.wake_agent().await?;
            assert_eq!(turn_starts(&mut calls).len(), 1, "retained events have one review without a phantom reply");
            assert!(lock(&reopened.triage.answered).is_empty());
            Ok::<_, anyhow::Error>(())
        }.await;
        cleanup(reopened, dir).await?;
        result
    }

    #[tokio::test]
    async fn triage_private_claims_roundtrip_but_external_and_malformed_metadata_are_rejected()
    -> Result<()> {
        let (node, admin, dir) = node().await?;
        let result = async {
            with_agent(&node);
            bot(&node, &admin, "c", "前端", &[]).await?;
            let ingress = turn("codec", "turn", "/tmp");
            let mut forged = serde_json::to_value(&ingress)?;
            forged["_triage"] = json!({"attempt":"forged","boot":"forged","state":"in_flight"});
            assert!(
                serde_json::from_value::<AgentEvent>(forged).is_err(),
                "external ingress cannot supply an internal claim"
            );
            node.ingest_agent_event(ingress).await?;
            let network = node.network_id().await?;
            let mut store = node.store.lock().await;
            let pending = store.triage_events(None)?;
            let raw: String =
                store
                    .connection
                    .query_row("SELECT event FROM triage_queue", [], |row| row.get(0))?;
            assert!(
                serde_json::from_str::<AgentEvent>(&raw).is_ok(),
                "Pending is the ordinary final-domain event shape"
            );
            let TriageAdmission::Claimed(attempt) =
                store.admit_triage(&network, &node.boot, 1, 100, now_ms())?
            else {
                bail!("codec claim");
            };
            assert_eq!(
                store.triage_events(Some((&node.boot, attempt.id())))?,
                pending,
                "InFlight preserves the event exactly"
            );
            let encoded: String =
                store
                    .connection
                    .query_row("SELECT event FROM triage_queue", [], |row| row.get(0))?;
            let mut invalid: Value = serde_json::from_str(&encoded)?;
            invalid["_triage"]["unexpected"] = json!(true);
            store
                .connection
                .execute("UPDATE triage_queue SET event=?", [invalid.to_string()])?;
            assert!(
                store.triage_events(None).is_err(),
                "the private claim is parsed strictly"
            );
            store
                .connection
                .execute("UPDATE triage_queue SET event=?", [&encoded])?;
            let unknown = Err(anyhow::anyhow!("unknown"));
            store.finish_triage(attempt, &unknown)?;
            assert_eq!(
                store.unknown_triage_events(None)?,
                pending,
                "Unknown preserves the event exactly"
            );
            assert!(store.triage_events(None)?.is_empty());
            store.connection.execute(
                "UPDATE triage_seen SET ts_ms=?",
                [now_ms() - SEEN_TTL_MS - 1],
            )?;
            drop(store);
            lock(&node.triage.config).scan_codex_sessions = false;
            node.scan_sources().await?;
            assert_eq!(
                node.ingest_agent_event(turn("codec", "turn", "/tmp"))
                    .await?["status"],
                "duplicate",
                "a queued Unknown event keeps its deduplication marker past the normal seen TTL"
            );
            let status = node.triage_settings().await?["status"].clone();
            let diagnostics = node.diagnostics(&network, (None, None)).await?;
            assert_eq!(
                diagnostics["triage"], status,
                "settings and diagnostics derive one budget snapshot"
            );
            assert_eq!(status["unknown_events"], 1);
            assert!(!status.to_string().contains("/tmp"));
            assert!(!status.to_string().contains("codec"));
            Ok::<_, anyhow::Error>(())
        }
        .await;
        cleanup(node, dir).await?;
        result
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn ordinary_coordination_refreshes_source_access_before_taking_the_turn_lock()
    -> Result<()> {
        use std::os::unix::fs::PermissionsExt;
        let (mut node, _admin, dir) = node().await?;
        let runtime = Arc::get_mut(&mut node).context("exclusive fixture runtime")?;
        let source = dir.join("source-codex");
        config::private_dir(&source)?;
        runtime.host.codex_home = Some(source.clone());
        let program = dir.join("controlled-codex");
        config::private_write(&program, b"#!/bin/sh\nexit 0\n")?;
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o700))?;
        let original_server = {
            let mut local = runtime.providers.lock().unwrap();
            let original = local.switches.codex_app_server.clone();
            local.bins.insert(Provider::Codex, program);
            local.switches.codex_app_server.mode = crate::providers::AppServerMode::Spawn;
            local.switches.codex_app_server.socket = None;
            original
        };
        with_agent(&node);
        let result = async {
            let roots = crate::providers::ProviderRoots::new(&dir);
            for (index, access) in ["synthetic-first-access", "synthetic-next-access"]
                .into_iter()
                .enumerate()
            {
                config::private_write(
                    &source.join("auth.json"),
                    &serde_json::to_vec(&json!({"tokens":{"access_token":access,
                        "refresh_token":"synthetic-source-owned-refresh","account_id":"fixture"}}))?,
                )?;
                let active = (index == 1).then(|| node.triage.turn.try_lock().unwrap());
                tokio::time::timeout(Duration::from_secs(5), node.wake_agent()).await??;
                let local = node.providers();
                assert_eq!(local.codex_external_auth().context("borrowed access")?.access_token, access);
                assert_eq!(
                    node.pending_provider_launches.load(Ordering::SeqCst),
                    active.is_some(),
                    "a busy turn defers the launch without blocking access refresh"
                );
                drop(active);
                tokio::time::timeout(Duration::from_secs(5), node.wake_agent()).await??;
                assert!(!node.pending_provider_launches.load(Ordering::SeqCst));
                let source_login: Value = serde_json::from_slice(&std::fs::read(source.join("auth.json"))?)?;
                assert_eq!(source_login["tokens"]["refresh_token"], "synthetic-source-owned-refresh");
                assert!(roots.codex_home.join("local-login-access.json").is_file());
            }
            Ok::<_, anyhow::Error>(())
        }.await;
        node.providers.lock().unwrap().switches.codex_app_server = original_server;
        cleanup(node, dir).await?;
        result
    }

    #[tokio::test]
    async fn the_coordination_agents_thread_is_not_triaged() -> Result<()> {
        let (node, admin, dir) = node().await?;
        let result = async {
            with_agent(&node);
            bot(&node, &admin, "c", "前端", &[]).await?;
            let _requests = coordination_app_server(&node).await?;
            node.coordination_turn(None, |_| Ok("triage".into()))
                .await?;
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
            external_work_thread_fact(&node, &admin, "s1").await?;
            let secret = "fixture-webhook-secret-0123456789";
            let project = dir.join("hooked");
            let project_text = project.to_string_lossy();
            let elsewhere = dir.join("elsewhere");
            let settings = node
                .configure_triage(json!({"default_outlet":"record","projects":[{"prefix":project_text,"outlet":"both"}],
                    "webhook_url":url,"scan_codex_sessions":false,"webhook_secret":secret}))
                .await?;
            assert_eq!(settings["webhook_secret_set"], true);
            assert!(!settings.to_string().contains(secret));
            assert!(!std::fs::read_to_string(dir.join("config.json"))?.contains(secret));

            let first = node.ingest_agent_event(turn("s1", "t1", &format!("{project_text}/app"))).await?;
            assert_eq!(first["status"], "accepted");
            let scanned = AgentEvent { source: triage::Source::SessionScan, status: Some("completed".into()), ..turn("s1", "t1", &format!("{project_text}/app")) };
            assert_eq!(node.ingest_agent_event(scanned).await?["status"], "duplicate");
            let test = node.ingest_agent_event(AgentEvent { test: true, ..turn("s9", "t9", "/x") }).await?;
            assert_eq!(test["test"], true);
            node.ingest_agent_event(turn("s1", "t2", &format!("{project_text}/app"))).await?;
            node.ingest_agent_event(turn("s2", "t1", elsewhere.to_string_lossy().as_ref())).await?;
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
            assert_eq!(body["answer"], json!({"url":decision_url(&node.config)?,"device_id":node.identity.device_id()?,"question_id":question}));
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
            external_work_thread_fact(&node, &admin, "s1").await?;
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

    #[tokio::test]
    async fn provider_paths_a_bot_proposes_are_written_only_after_the_user_allows() -> Result<()> {
        let (node, admin, dir) = node().await?;
        let result = async {
            bot(&node, &admin, "c", "前端", &[]).await?;
            let network = node.network_id().await?;
            for id in ["m-1", "m-2", "m-3", "m-4", "m-5", "m-6"] {
                main_input_fact(&node, &admin, "c", id).await?;
            }
            let codex = dir.join("fixture-codex");
            std::fs::create_dir_all(codex.join("sessions"))?;
            std::fs::create_dir_all(codex.join("archived"))?;
            let saved = || Config::load(&dir).map(|config| config.triage.codex_sessions);

            let asked = node
                .propose_provider_config(
                    &network,
                    "c",
                    "message:m-1",
                    json!({"codex_sessions": codex.join("sessions")}),
                )
                .await?;
            assert_eq!(asked["status"], "asked");
            let question = asked["question_id"].as_str().unwrap().to_owned();
            let card = unanswered(&node).await?;
            assert_eq!(card.len(), 1);
            assert!(
                !card[0].to_string().contains(&*dir.to_string_lossy()),
                "the card names no full local path"
            );
            assert!(
                node.apply_provider_config(&network, "c", &question)
                    .await
                    .is_err(),
                "nothing is written before an answer"
            );
            assert_eq!(saved()?, None);

            answer(&node, &question, "不允许").await?;
            assert_eq!(
                node.apply_provider_config(&network, "c", &question).await?["status"],
                "declined"
            );
            assert_eq!(saved()?, None, "a declined proposal writes nothing");
            assert_eq!(
                node.triage.config().codex_sessions,
                Some(dir.join("providers/codex/sessions"))
            );

            let asked = node
                .propose_provider_config(
                    &network,
                    "c",
                    "message:m-2",
                    json!({"codex_sessions": codex.join("archived")}),
                )
                .await?;
            let question = asked["question_id"].as_str().unwrap().to_owned();
            assert!(
                node.apply_provider_config(&network, "other", &question)
                    .await
                    .is_err(),
                "another chat cannot apply it"
            );
            answer(&node, &question, "允许").await?;
            let applied = node.apply_provider_config(&network, "c", &question).await?;
            assert_eq!(applied["status"], "applied");
            assert_eq!(saved()?, Some(codex.join("archived")));
            assert_eq!(
                node.triage.config().codex_sessions,
                Some(codex.join("archived"))
            );
            assert_eq!(
                node.apply_provider_config(&network, "c", &question).await?,
                applied,
                "a retry reads the first outcome back"
            );
            assert_eq!(
                node.propose_provider_config(
                    &network,
                    "c",
                    "message:m-3",
                    json!({"codex_sessions": codex.join("archived")})
                )
                .await?["status"],
                "unchanged"
            );
            assert!(
                node.propose_provider_config(
                    &network,
                    "c",
                    "message:m-4",
                    json!({"codex_sessions": "relative"})
                )
                .await
                .is_err(),
                "only absolute paths that exist are proposed"
            );

            let projects = dir.join("projects");
            std::fs::create_dir_all(&projects)?;
            let projects = projects.canonicalize()?;
            let asked = node
                .propose_provider_config(&network, "c", "message:m-5", json!({"roots": [projects]}))
                .await?;
            let question = asked["question_id"].as_str().unwrap().to_owned();
            assert!(!node.settings_snapshot().roots.contains(&projects));
            answer(&node, &question, "允许").await?;
            assert_eq!(
                node.apply_provider_config(&network, "c", &question).await?["roots_added"],
                1
            );
            assert!(node.settings_snapshot().roots.contains(&projects));
            assert!(
                Config::load(&dir)?.roots.contains(&projects),
                "the folder is saved"
            );
            assert_eq!(
                node.propose_provider_config(
                    &network,
                    "c",
                    "message:m-6",
                    json!({"roots": [projects]})
                )
                .await?["status"],
                "unchanged"
            );
            Ok::<_, anyhow::Error>(())
        }
        .await;
        cleanup(node, dir).await?;
        result
    }

    fn controlled_pi(node: &Runtime, dir: &Path, mode: &str) -> Result<()> {
        let name = if cfg!(windows) {
            "fake-pi.cmd"
        } else {
            "fake-pi"
        };
        let bin = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures")
            .join(name);
        vibeface_bridge::platform::program::ensure_runnable(&bin, "controlled pi fixture")?;
        let mut providers = node.providers.lock().unwrap();
        providers.switches.execution = [Provider::Pi].into();
        providers.bins.insert(Provider::Pi, bin);
        providers.env.apply(
            Provider::Pi,
            BTreeMap::from([
                (
                    "PI_CODING_AGENT_SESSION_DIR".into(),
                    Some(dir.join("pi-sessions").display().to_string()),
                ),
                (
                    "FAKE_PI_LOG".into(),
                    Some(dir.join("pi.log").display().to_string()),
                ),
                ("FAKE_PI_MODE".into(), Some(mode.into())),
            ]),
        )?;
        Ok(())
    }

    fn pi_fixture_log(dir: &Path) -> Result<Vec<Value>> {
        let text = std::fs::read_to_string(dir.join("pi.log")).unwrap_or_default();
        text.lines()
            .map(|line| Ok(serde_json::from_str(line)?))
            .collect()
    }

    async fn queued_task(
        node: &Runtime,
        admin: &LocalIdentity,
        scope: &str,
        call: &str,
        provider: Provider,
        model: Option<&str>,
    ) -> Result<crate::work::WorkRequest> {
        let trigger = main_input_fact(node, admin, scope, "origin").await?;
        let request = crate::work::WorkRequest {
            work_id: crate::work::WorkSpec::id(scope, &trigger, call),
            conversation: scope.into(),
            trigger,
            target: node.identity.device_id()?,
            dispatcher: LocalIdentity::generate().device_id()?,
            call_ref: call.into(),
            kind: crate::work::RequestedWorkKind::Task,
            model: None,
            cwd: "/tmp".into(),
            title: "task".into(),
            prompt: "controlled task".into(),
            continues: None,
            parent: None,
            thread: None,
        };
        let network = node.network_id().await?;
        node.store.lock().await.accept_work(
            &node.identity,
            &network,
            request.clone(),
            ResolvedModel {
                provider,
                model: model.map(str::to_owned),
            },
            now_ms(),
        )?;
        Ok(request)
    }

    async fn assert_refused_task(
        node: &Runtime,
        network: &str,
        request: &crate::work::WorkRequest,
        kind: FailureKind,
    ) -> Result<()> {
        use crate::scheduling::{JobState, TurnExecution};
        let store = node.store.lock().await;
        let job = store
            .scheduler_job(
                &request.conversation,
                &crate::scheduling::task_trigger(&request.work_id),
            )?
            .context("task job")?;
        assert!(
            matches!(&job.state, JobState::Failed { failure, .. } if failure.kind == kind),
            "{job:?}"
        );
        let runtime = job.runtime.as_ref().context("runtime")?;
        assert_eq!(runtime.provider, Provider::Pi);
        assert_eq!(runtime.model.as_deref(), Some("fake/frozen"));
        assert_eq!(runtime.execution, TurnExecution::NotStarted);
        assert_eq!(
            runtime.stage,
            vibeface_bridge::failure::ProviderStage::Validate
        );
        assert_eq!(runtime.switched_from, None);
        assert!(job.retryable() && !job.pending_check && !job.tool_calls);
        assert_eq!(store.work_records()?.len(), 1);
        assert_eq!(
            store
                .scheduler_jobs(&request.conversation)?
                .iter()
                .filter(|job| job.kind == crate::scheduling::TriggerKind::AgentTask)
                .count(),
            1
        );
        let events = store.context(network, &request.conversation, None)?.events;
        assert_eq!(events.iter().filter(|event| matches!(&event.event.body, EventBody::WorkDispatched { work_id, .. } if work_id == &request.work_id)).count(), 1);
        assert_eq!(events.iter().filter(|event| matches!(&event.event.body, EventBody::WorkFinished { work_id, .. } if work_id == &request.work_id)).count(), 1);
        assert_eq!(events.iter().filter(|event| matches!(&event.event.body, EventBody::WorkProgress { work_id, .. } if work_id == &request.work_id)).count(), 0);
        Ok(())
    }

    #[tokio::test]
    async fn thread_dispatch_retries_read_the_durable_work_before_execution_and_native_state()
    -> Result<()> {
        use futures_util::{SinkExt as _, StreamExt as _};
        let (node, admin, dir) = node().await?;
        let native_dirs = [
            "explicit",
            "implicit",
            "explicit-default",
            "implicit-default",
        ]
        .into_iter()
        .map(|call| {
            let thread = format!("t-{call}");
            let cwd = dir.join(&thread);
            std::fs::create_dir(&cwd)?;
            Ok((thread, cwd.canonicalize()?.display().to_string()))
        })
        .collect::<Result<BTreeMap<_, _>>>()?;
        let served_dirs = native_dirs.clone();
        let requests = Arc::new(std::sync::Mutex::new(Vec::<Value>::new()));
        let turns = Arc::new(std::sync::Mutex::new(BTreeMap::<String, Value>::new()));
        let mut listener = vibeface_bridge::platform::ipc::Listener::bind(
            &vibeface_bridge::platform::ipc::Endpoint::from_path(&named_socket(&node))?,
        )
        .await?;
        let seen = requests.clone();
        let server = tokio::spawn(async move {
            let mut sessions = tokio::task::JoinSet::new();
            while let Ok(stream) = listener.accept().await {
                let seen = seen.clone();
                let turns = turns.clone();
                let native_dirs = served_dirs.clone();
                sessions.spawn(async move {
                    let Ok(mut server) = tokio_tungstenite::accept_async(stream).await else { return; };
                    let mut direct = false;
                    while let Some(Ok(message)) = server.next().await {
                        let Ok(request) = serde_json::from_slice::<Value>(&message.into_data()) else { continue; };
                        if request["method"] == "initialize" { direct = request["params"]["clientInfo"]["title"] == "Vibeface Agent CLI"; }
                        if direct || request["method"].as_str().is_some_and(|method| method.starts_with("turn/")) { seen.lock().unwrap().push(request.clone()); }
                        let thread = request["params"]["threadId"].as_str().unwrap_or_default();
                        let result = {
                        let mut turns = turns.lock().unwrap();
                        match request["method"].as_str() {
                            Some("initialize") => json!({"userAgent":"codex/fixture"}),
                            Some("thread/read") if thread == "missing" => json!({"thread":null}),
                            Some("thread/read") if thread == "sub" => json!({"thread":{"id":"sub","source":{"subAgent":{}},"parentThreadId":"lead"}}),
                            Some("turn/start") => {
                                let turn = json!({"id":format!("turn-{thread}"),"status":"inProgress","items":[{"type":"userMessage","clientId":request["params"]["clientUserMessageId"]}]});
                                turns.insert(thread.into(), turn.clone());
                                json!({"turn":turn})
                            }
                            Some("turn/steer") => {
                                let turn = turns.get_mut(thread).unwrap();
                                assert_eq!(request["params"]["expectedTurnId"], turn["id"]);
                                turn["items"].as_array_mut().unwrap().push(json!({"type":"userMessage","clientId":request["params"]["clientUserMessageId"]}));
                                json!({"turnId":turn["id"]})
                            }
                            Some("thread/read" | "thread/resume") => json!({"thread":{"id":thread,"cwd":native_dirs.get(thread),"source":"cli","status":{"type":if turns.contains_key(thread) {"active"} else {"idle"}},"turns":turns.get(thread).cloned().into_iter().collect::<Vec<_>>()}}),
                            _ => json!({}),
                        }
                        };
                        if request.get("id").is_some() {
                            let _ = server.send(json!({"id":request["id"],"result":result}).to_string().into()).await;
                        }
                    }
                });
            }
        });
        let result = async {
            with_agent(&node);
            node.settings.lock().unwrap().roots = vec![dir.canonicalize()?];
            bot(&node, &admin, "c", "Bot", &[]).await?;
            let network = node.network_id().await?;
            let (trigger, attempt) = start_reply(&node, &admin, "m1").await?;
            node.register_manager_attempt_fixture("thread-retry-token".into(), Access { network_id: network.clone(), human: false, conversation_id: Some("c".into()), manager: None }, &attempt, &trigger, Provider::Codex).await?;
            let access = node.agent_tokens.lock().await.get("thread-retry-token").cloned().context("access")?;
            let mut original = Vec::new();
            for (call, explicit, model) in [("explicit", true, Some("frozen")), ("implicit", false, Some("frozen")), ("explicit-default", true, None), ("implicit-default", false, None)] {
                {
                    let mut providers = node.providers.lock().unwrap();
                    match model { Some(model) => { providers.switches.execution_models.insert(Provider::Codex, model.into()); }, None => { providers.switches.execution_models.remove(&Provider::Codex); } }
                }
                let mut args = json!({"thread":format!("t-{call}"),"prompt":"\n follow up \nmore","call_ref":call});
                if explicit { args["agent"] = json!("codex"); }
                let first = node.tool(access.clone(), "dispatch_work", args.clone()).await?;
                let expected = json!({"provider":"codex","model":model});
                let work = node.store.lock().await.works(&network, "c")?.into_iter().find(|work| first["work_id"].as_str() == Some(work.work_id.as_str())).context("accepted work")?;
                assert_eq!(work.cwd, native_dirs[&format!("t-{call}")]);
                assert_eq!(work.title, "follow up");
                assert_eq!(json!(work.resolved), expected);
                original.push((args, first["work_id"].clone(), expected));
            }
            let supplement = json!({"agent":"codex","thread":"t-explicit","prompt":"same turn supplement","call_ref":"supplement"});
            let second = node.tool(access.clone(), "dispatch_work", supplement.clone()).await?;
            let second_id = second["work_id"].as_str().context("supplement work")?;
            assert_eq!(node.store.lock().await.work_native_execution(&network, second_id)?.context("native permit")?.turn_id, "turn-t-explicit");
            assert_ne!(second["work_id"], original[0].1);
            assert_eq!(node.store.lock().await.work_record(second_id)?.context("supplement")?.state.handle().context("supplement handle")?.turn_id, "turn-t-explicit");
            node.bind_work(&network, "c", second_id, vibeface_bridge::codex::StartReport::Turn { turn_id:"another-turn".into() }).await.expect_err("a mismatched report must remain unknown");
            assert!(matches!(node.store.lock().await.work_record(second_id)?.context("supplement")?.state, crate::work::WorkState::Unverified { .. }));
            original.push((supplement, second["work_id"].clone(), json!({"provider":"codex","model":null})));
            for (thread, reason) in [("missing", "was not returned"), ("sub", "parent thread lead")] {
                let failure = node.tool(access.clone(), "dispatch_work", json!({"thread":thread,"prompt":"p","call_ref":thread})).await.expect_err("invalid native thread must not create work");
                assert!(failure.to_string().contains(reason), "{failure:#}");
                assert_eq!(node.store.lock().await.work_records()?.len(), 5);
            }
            node.signal_work(&network, original[0].1.as_str().context("work id")?, crate::work::Signal::TurnCompleted { turn_id: "turn-t-explicit".into(), status: vibeface_bridge::turn::TurnStatus::Completed, text: "done".into(), error: None }).await?;
            node.signal_work(&network, original[1].1.as_str().context("work id")?, crate::work::Signal::Disconnected).await?;
            assert!(matches!(node.store.lock().await.work_record(original[0].1.as_str().context("work id")?)?.context("finished")?.state, crate::work::WorkState::Finished { .. }));
            assert!(matches!(node.store.lock().await.work_record(original[1].1.as_str().context("work id")?)?.context("unknown")?.state, crate::work::WorkState::Unverified { .. }));
            {
                let _controls = node.manager_controls.lock().await;
                node.providers.lock().unwrap().switches.execution.clear();
                node.providers.lock().unwrap().switches.execution_models.insert(Provider::Codex, "changed-default".into());
                node.settings.lock().unwrap().manager = None;
            }
            let before = requests.lock().unwrap().clone();
            assert_eq!(before.iter().filter(|request| request["method"] == "turn/start").count(), 4);
            assert_eq!(before.iter().filter(|request| request["method"] == "turn/steer").count(), 1);
            for (args, work_id, expected) in &original {
                for (tool, input) in [("dispatch_work", args.clone()), ("tool_call", json!({"tool":"dispatch_work","arguments":args,"call_ref":args["call_ref"]}))] {
                    let retried = node.tool(access.clone(), tool, input).await?;
                    assert_eq!(&retried["work_id"], work_id);
                    assert_eq!(&retried["resolved"], expected);
                    assert_eq!(retried["may_send"], false);
                }
                for (field, value) in [("title", json!("changed title")), ("prompt", json!("changed prompt")), ("thread", json!("changed-thread")), ("model", json!("changed model")), ("agent", json!("claude"))] {
                    let mut changed = args.clone(); changed[field] = value;
                    let failure = node.tool(access.clone(), "dispatch_work", changed).await.expect_err("Store must reject changed payloads");
                    assert!(failure.to_string().contains("different request"), "{field}: {failure:#}");
                }
                let mut changed_cwd = args.clone(); changed_cwd["cwd"] = json!("/still-ignored");
                assert_eq!(node.tool(access.clone(), "dispatch_work", changed_cwd).await?["work_id"], *work_id);
            }
            assert_eq!(*requests.lock().unwrap(), before);
            let failure = node.tool(access.clone(), "dispatch_work", json!({"thread":"fresh","prompt":"p","call_ref":"no-ready"})).await.expect_err("ready None must not create work");
            assert!(failure.to_string().contains("no execution agent"), "{failure:#}");
            assert_eq!(*requests.lock().unwrap(), before);
            server.abort();
            let _ = std::fs::remove_file(named_socket(&node));
            node.providers.lock().unwrap().switches.execution.insert(Provider::Codex);
            for (args, work_id, expected) in &original {
                let retried = tokio::time::timeout(Duration::from_secs(2), node.tool(access.clone(), "dispatch_work", args.clone())).await??;
                assert_eq!(&retried["work_id"], work_id);
                assert_eq!(&retried["resolved"], expected);
                assert_eq!(retried["may_send"], false);
            }
            let store = node.store.lock().await;
            assert_eq!(store.work_records()?.len(), 5);
            assert_eq!(store.context(&network, "c", None)?.events.iter().filter(|event| matches!(event.event.body, EventBody::WorkDispatched { .. })).count(), 5);
            drop(store);
            node.finish_manager_attempt(&attempt).await;
            Ok::<_, anyhow::Error>(())
        }.await;
        server.abort();
        node.shutdown().await;
        cleanup(node, dir).await?;
        result
    }

    #[tokio::test]
    async fn admitted_local_retries_keep_the_frozen_selection_after_models_and_permissions_change()
    -> Result<()> {
        let (node, admin, dir) = node().await?;
        let result = async {
            with_agent(&node);
            controlled_pi(&node, &dir, "reply")?;
            node.providers.lock().unwrap().switches.execution_models.insert(Provider::Pi, "fake/frozen".into());
            node.settings.lock().unwrap().roots = vec![dir.clone()];
            bot(&node, &admin, "c", "Bot", &[]).await?;
            let network = node.network_id().await?;
            let (trigger, attempt) = start_reply(&node, &admin, "m1").await?;
            node.register_manager_attempt_fixture("retry-token".into(), Access { network_id: network.clone(), human: false, conversation_id: Some("c".into()), manager: None }, &attempt, &trigger, Provider::Codex).await?;
            let access = node.agent_tokens.lock().await.get("retry-token").cloned().context("access")?;
            let args = |call: &str, explicit: bool| -> Result<Value> {
                let cwd = dir.join(call);
                std::fs::create_dir(&cwd)?;
                let mut args = json!({"cwd":cwd.display().to_string(),"call_ref":call,"title":"task","prompt":"controlled work"});
                if explicit { args["agent"] = json!("pi"); }
                Ok(args)
            };
            let mut original = Vec::new();
            for (call, explicit) in [("explicit", true), ("implicit", false), ("provider-default", false)] {
                if call == "provider-default" { node.providers.lock().unwrap().switches.execution_models.remove(&Provider::Pi); }
                let args = args(call, explicit)?;
                let first = node.tool(access.clone(), "track_work", args.clone()).await?;
                assert_eq!(first["may_send"], false);
                assert_eq!(first["resolved"], json!({"provider":"pi","model":if call == "provider-default" { Value::Null } else { json!("fake/frozen") }}));
                original.push((args, first));
            }
            tokio::time::timeout(Duration::from_secs(10), async {
                loop {
                    if node.store.lock().await.work_records()?.iter().all(|run| run.state.finished()) { break Ok::<_, anyhow::Error>(()); }
                    tokio::task::yield_now().await;
                }
            }).await??;
            {
                let _controls = node.manager_controls.lock().await;
                let mut providers = node.providers.lock().unwrap();
                providers.switches.execution.clear();
                providers.switches.execution_models.insert(Provider::Pi, "fake/new-default".into());
                node.settings.lock().unwrap().manager = None;
            }
            for (args, first) in &original {
                let retried = node.tool(access.clone(), "track_work", args.clone()).await?;
                assert_eq!(retried["resolved"], first["resolved"]);
                assert_eq!(retried["may_send"], false);
                assert_eq!(retried["work_id"], first["work_id"]);
                for (key, value) in [("agent", json!("codex")), ("model", json!("changed")), ("prompt", json!("changed"))] {
                    let mut changed = args.clone(); changed[key] = value;
                    assert!(node.tool(access.clone(), "track_work", changed).await.is_err());
                }
            }
            assert_eq!(pi_fixture_log(&dir)?.iter().filter(|entry| entry.get("prompt").is_some()).count(), 3);
            let store = node.store.lock().await;
            assert_eq!(store.work_records()?.len(), 3);
            assert_eq!(store.context(&network, "c", None)?.events.iter().filter(|event| matches!(event.event.body, EventBody::WorkDispatched { .. })).count(), 3);
            drop(store);
            node.finish_manager_attempt(&attempt).await;
            Ok::<_, anyhow::Error>(())
        }.await;
        node.shutdown().await;
        cleanup(node, dir).await?;
        result
    }

    #[tokio::test]
    async fn queued_tasks_are_refused_before_manager_launch_and_active_session_gates() -> Result<()>
    {
        for (via_tick, current) in [
            (false, None),
            (false, Some(Provider::Claude)),
            (true, None),
            (true, Some(Provider::Claude)),
        ] {
            let (node, admin, dir) = node().await?;
            let result = async {
                with_agent(&node);
                controlled_pi(&node, &dir, "reply")?;
                bot(&node, &admin, "c", "Bot", &[]).await?;
                let network = node.network_id().await?;
                let (human_trigger, _) = claim_reply(&node, &admin, "c", "human", crate::scheduler::boot_id()).await?;
                let active = node.store.lock().await.scheduler_job("c", &human_trigger)?.context("active")?;
                let before = node.store.lock().await.connection.query_row("SELECT json_array(active,permission_version,session_generation,sdk_config) FROM scheduler_sessions WHERE conversation_id='c'", [], |row| row.get::<_, String>(0))?;
                let request = queued_task(&node, &admin, "c", "early-refusal", Provider::Pi, Some("fake/frozen")).await?;
                {
                    let _controls = node.manager_controls.lock().await;
                    node.settings.lock().unwrap().manager = current.map(|provider| crate::config::ManagerSettings { provider, workspace: dir.clone(), model: "changed".into(), reasoning_effort: None, backup: crate::scheduler::BackupPolicy { automatic: true, candidates: vec![crate::scheduler::BackupModel { provider: Provider::Pi, model: "fake/backup".into(), reasoning_effort: None }] } });
                    node.pending_provider_launches.store(true, Ordering::SeqCst);
                }
                if via_tick {
                    assert!(node.schedule_workers.lock().await.insert((network.clone(), crate::scheduling::SessionKey { conversation_id: "c".into(), scope: crate::scheduling::SessionScope::Main })));
                    node.schedule_tick().await?;
                } else {
                    crate::scheduler::run(node.clone(), Access { network_id: network.clone(), human: false, conversation_id: Some("c".into()), manager: None }, "c".into(), crate::scheduling::SessionScope::Task { work_id: request.work_id.clone() }).await?.context("refusal invocation")?;
                }
                assert_refused_task(&node, &network, &request, if current.is_none() { FailureKind::NoBot } else { FailureKind::ExecutionDisabled }).await?;
                let refused = node.store.lock().await.scheduler_job("c", &crate::scheduling::task_trigger(&request.work_id))?.context("refusal")?;
                node.finish_incompatible_tasks(&network, None).await?;
                assert_eq!(node.store.lock().await.scheduler_job("c", &refused.trigger)?.context("refusal")?.runtime, refused.runtime);
                assert_eq!(serde_json::to_value(node.store.lock().await.scheduler_job("c", &human_trigger)?.context("active")?)?, serde_json::to_value(active)?);
                let after = node.store.lock().await.connection.query_row("SELECT json_array(active,permission_version,session_generation,sdk_config) FROM scheduler_sessions WHERE conversation_id='c'", [], |row| row.get::<_, String>(0))?;
                assert_eq!(before, after);
                assert!(pi_fixture_log(&dir)?.is_empty());
                assert!(node.agent_tokens.lock().await.is_empty());
                let retried = node.accept_dispatch(&network, request.clone(), &Value::Null).await?;
                assert!(!retried.created);
                assert_eq!(retried.spec.resolved.model.as_deref(), Some("fake/frozen"));
                let peer_retry = node.accept_remote_dispatch(&network, &request.dispatcher, &json!({"spec":request})).await?;
                assert_eq!(peer_retry["work_id"], request.work_id);
                for resolved in [Value::Null, json!({"provider":"pi","model":"forged"})] {
                    let mut input = json!({"spec":request}); input["spec"]["resolved"] = resolved;
                    assert!(node.accept_remote_dispatch(&network, &request.dispatcher, &input).await.is_err());
                }
                assert!(node.accept_remote_dispatch(&network, &node.identity.device_id()?, &json!({"spec":request})).await.is_err());
                if via_tick {
                    assert!(node.schedule_workers.lock().await.remove(&(network.clone(), crate::scheduling::SessionKey { conversation_id: "c".into(), scope: crate::scheduling::SessionScope::Main })));
                }
                Ok::<_, anyhow::Error>(())
            }.await;
            cleanup(node, dir).await?;
            result?;
        }
        Ok(())
    }

    #[tokio::test]
    async fn a_task_disabled_between_claim_and_start_finishes_without_a_token_or_sdk_call()
    -> Result<()> {
        for current in [None, Some(Provider::Claude)] {
            let (node, admin, dir) = node().await?;
            let result = async {
                with_agent(&node);
                controlled_pi(&node, &dir, "reply")?;
                let mut manager = node.manager().context("manager")?;
                manager.provider = Provider::Pi;
                manager.model = "fake/current".into();
                manager.workspace = dir.clone();
                manager.backup = crate::scheduler::BackupPolicy {
                    automatic: true,
                    candidates: vec![crate::scheduler::BackupModel {
                        provider: Provider::Pi,
                        model: "fake/backup".into(),
                        reasoning_effort: None,
                    }],
                };
                node.settings.lock().unwrap().manager = Some(manager.clone());
                node.tool_url.set("http://127.0.0.1:1".into()).unwrap();
                bot(&node, &admin, "c", "Bot", &[]).await?;
                let network = node.network_id().await?;
                let request = queued_task(
                    &node,
                    &admin,
                    "c",
                    "barrier",
                    Provider::Pi,
                    Some("fake/frozen"),
                )
                .await?;
                let claimed = Arc::new(tokio::sync::Notify::new());
                let resume = Arc::new(tokio::sync::Notify::new());
                *node.claim_to_start_barrier.lock().unwrap() =
                    Some((claimed.clone(), resume.clone()));
                let task_node = node.clone();
                let task_network = network.clone();
                let task_scope = crate::scheduling::SessionScope::Task {
                    work_id: request.work_id.clone(),
                };
                let running = tokio::spawn(async move {
                    crate::scheduler::run(
                        task_node,
                        Access {
                            network_id: task_network,
                            human: false,
                            conversation_id: Some("c".into()),
                            manager: None,
                        },
                        "c".into(),
                        task_scope,
                    )
                    .await
                });
                tokio::time::timeout(Duration::from_secs(5), claimed.notified()).await?;
                let initial = node
                    .store
                    .lock()
                    .await
                    .scheduler_job("c", &crate::scheduling::task_trigger(&request.work_id))?
                    .context("claimed")?;
                assert_eq!(
                    initial
                        .runtime
                        .as_ref()
                        .context("initial runtime")?
                        .model
                        .as_deref(),
                    Some("fake/frozen")
                );
                {
                    let _controls = node.manager_controls.lock().await;
                    node.settings.lock().unwrap().manager =
                        current.map(|provider| crate::config::ManagerSettings {
                            provider,
                            ..manager
                        });
                }
                resume.notify_one();
                tokio::time::timeout(Duration::from_secs(10), running)
                    .await???
                    .context("finished refusal")?;
                assert_refused_task(
                    &node,
                    &network,
                    &request,
                    if current.is_none() {
                        FailureKind::NoBot
                    } else {
                        FailureKind::ExecutionDisabled
                    },
                )
                .await?;
                assert!(node.agent_tokens.lock().await.is_empty());
                assert!(node.manager_controls.lock().await.is_empty());
                assert!(node.store.lock().await.pending_checks()?.is_empty());
                assert!(pi_fixture_log(&dir)?.is_empty());
                let active: Option<String> = node.store.lock().await.connection.query_row(
                    "SELECT active FROM scheduler_sessions WHERE conversation_id='c'",
                    [],
                    |row| row.get(0),
                )?;
                assert_eq!(active, None);
                Ok::<_, anyhow::Error>(())
            }
            .await;
            node.shutdown().await;
            cleanup(node, dir).await?;
            result?;
        }
        Ok(())
    }

    #[tokio::test]
    async fn task_registration_that_cannot_mark_started_leaves_no_token_and_preserves_unknown_evidence()
    -> Result<()> {
        let (node, admin, dir) = node().await?;
        let result = async {
            with_agent(&node);
            controlled_pi(&node, &dir, "reply")?;
            let mut manager = node.manager().context("manager")?;
            manager.provider = Provider::Pi;
            manager.workspace = dir.clone();
            node.settings.lock().unwrap().manager = Some(manager);
            node.tool_url.set("http://127.0.0.1:1".into()).unwrap();
            bot(&node, &admin, "c", "Bot", &[]).await?;
            let network = node.network_id().await?;
            let request = queued_task(
                &node,
                &admin,
                "c",
                "mark-failure",
                Provider::Pi,
                Some("fake/frozen"),
            )
            .await?;
            let claimed = Arc::new(tokio::sync::Notify::new());
            let resume = Arc::new(tokio::sync::Notify::new());
            *node.claim_to_start_barrier.lock().unwrap() = Some((claimed.clone(), resume.clone()));
            let task_node = node.clone();
            let task_network = network.clone();
            let task_scope = crate::scheduling::SessionScope::Task {
                work_id: request.work_id.clone(),
            };
            let running = tokio::spawn(async move {
                crate::scheduler::run(
                    task_node,
                    Access {
                        network_id: task_network,
                        human: false,
                        conversation_id: Some("c".into()),
                        manager: None,
                    },
                    "c".into(),
                    task_scope,
                )
                .await
            });
            tokio::time::timeout(Duration::from_secs(5), claimed.notified()).await?;
            let trigger = crate::scheduling::task_trigger(&request.work_id);
            let job = node
                .store
                .lock()
                .await
                .scheduler_job("c", &trigger)?
                .context("claim")?;
            let attempt = job.state.attempt().context("attempt")?.id.clone();
            let before_registration = {
                let mut store = node.store.lock().await;
                store.mark_started("c", &trigger, &attempt)?;
                store.update_turn_runtime(
                    "c",
                    &trigger,
                    &attempt,
                    vibeface_bridge::failure::ProviderStage::TurnStart,
                    crate::scheduling::TurnExecution::Unknown,
                )?
            };
            resume.notify_one();
            tokio::time::timeout(Duration::from_secs(10), running)
                .await???
                .context("reconciled refusal")?;
            assert!(matches!(
                before_registration.state,
                crate::scheduling::JobState::Running { .. }
            ));
            assert_eq!(
                before_registration
                    .runtime
                    .as_ref()
                    .context("unknown runtime before registration")?
                    .execution,
                crate::scheduling::TurnExecution::Unknown
            );
            let job = node
                .store
                .lock()
                .await
                .scheduler_job("c", &trigger)?
                .context("job")?;
            assert!(matches!(
                job.state,
                crate::scheduling::JobState::Reconciling { .. }
            ));
            assert_eq!(
                job.runtime.as_ref().context("runtime")?.execution,
                crate::scheduling::TurnExecution::Unknown
            );
            assert!(job.pending_check);
            assert!(node.agent_tokens.lock().await.is_empty());
            assert!(node.manager_controls.lock().await.is_empty());
            assert!(pi_fixture_log(&dir)?.is_empty());
            assert_eq!(node.store.lock().await.pending_checks()?.len(), 1);
            Ok::<_, anyhow::Error>(())
        }
        .await;
        node.shutdown().await;
        cleanup(node, dir).await?;
        result
    }

    #[tokio::test]
    async fn task_turns_use_frozen_models_and_never_automatic_backup_but_human_turns_can()
    -> Result<()> {
        for (kind, accepted, mode) in [
            ("task", Some("fake/frozen"), "reply"),
            ("task-default", None, "reply"),
            ("task-failure", Some("fake/frozen"), "refuse"),
            ("human", None, "reply"),
        ] {
            let (node, admin, dir) = node().await?;
            let result = async {
                with_agent(&node);
                controlled_pi(&node, &dir, mode)?;
                let mut manager = node.manager().context("manager")?;
                manager.provider = if kind == "human" {
                    Provider::Claude
                } else {
                    Provider::Pi
                };
                manager.model = "fake/changed-current".into();
                manager.workspace = dir.clone();
                manager.backup = crate::scheduler::BackupPolicy {
                    automatic: true,
                    candidates: vec![crate::scheduler::BackupModel {
                        provider: Provider::Pi,
                        model: "fake/backup".into(),
                        reasoning_effort: None,
                    }],
                };
                node.settings.lock().unwrap().manager = Some(manager);
                node.providers
                    .lock()
                    .unwrap()
                    .bins
                    .insert(Provider::Claude, dir.join("missing-claude"));
                node.tool_url.set("http://127.0.0.1:1".into()).unwrap();
                bot(&node, &admin, "c", "Bot", &[]).await?;
                let network = node.network_id().await?;
                let trigger = if kind == "human" {
                    let posted = admin.attest(
                        &network,
                        "c",
                        EventBody::MessagePosted {
                            message_id: "human".into(),
                            role: Role::User,
                            text: "controlled request".into(),
                            attachments: vec![],
                            report_results: Some(true),
                            in_reply_to: None,
                            status: None,
                            failure: None,
                            answers: None,
                            attestation: None,
                        },
                    )?;
                    node.store.lock().await.accept_message(
                        &node.identity,
                        &network,
                        "c",
                        "human",
                        "controlled request",
                        vec![],
                        true,
                        posted.attestation().cloned().context("signature")?,
                        now_ms(),
                    )?;
                    "message:human".into()
                } else {
                    let request =
                        queued_task(&node, &admin, "c", kind, Provider::Pi, accepted).await?;
                    crate::scheduling::task_trigger(&request.work_id)
                };
                tokio::time::timeout(
                    Duration::from_secs(15),
                    crate::scheduler::run(
                        node.clone(),
                        Access {
                            network_id: network.clone(),
                            human: false,
                            conversation_id: Some("c".into()),
                            manager: None,
                        },
                        "c".into(),
                        match trigger.strip_prefix("task:") {
                            Some(work_id) => crate::scheduling::SessionScope::Task {
                                work_id: work_id.into(),
                            },
                            None => crate::scheduling::SessionScope::Main,
                        },
                    ),
                )
                .await??
                .context("turn")?;
                let job = node
                    .store
                    .lock()
                    .await
                    .scheduler_job("c", &trigger)?
                    .context("job")?;
                let runtime = job.runtime.as_ref().context("runtime")?;
                assert_eq!(runtime.provider, Provider::Pi);
                assert_eq!(
                    runtime.model.as_deref(),
                    if kind == "human" {
                        Some("fake/backup")
                    } else {
                        accepted
                    }
                );
                let log = pi_fixture_log(&dir)?;
                assert_eq!(
                    log.iter()
                        .filter(|entry| entry.get("prompt").is_some())
                        .count(),
                    1
                );
                let launches: Vec<_> = log
                    .iter()
                    .filter_map(|entry| entry["argv"].as_array())
                    .collect();
                assert_eq!(launches.len(), 1);
                let argv = launches[0];
                let model = argv
                    .iter()
                    .position(|arg| arg == "--model")
                    .map(|index| argv[index + 1].as_str().unwrap());
                assert_eq!(
                    model,
                    if kind == "human" {
                        Some("fake/backup")
                    } else {
                        accepted
                    }
                );
                if kind == "task-failure" {
                    assert!(matches!(
                        job.state,
                        crate::scheduling::JobState::Failed { .. }
                    ));
                    assert_eq!(
                        runtime.execution,
                        crate::scheduling::TurnExecution::NotStarted
                    );
                    assert_eq!(runtime.switched_from, None);
                } else {
                    assert!(
                        matches!(job.state, crate::scheduling::JobState::Completed { .. }),
                        "{job:?}"
                    );
                    assert_eq!(runtime.switched_from.is_some(), kind == "human");
                }
                Ok::<_, anyhow::Error>(())
            }
            .await;
            node.shutdown().await;
            cleanup(node, dir).await?;
            result?;
        }
        Ok(())
    }
}
