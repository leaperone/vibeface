//! Triage workers (design 4): capture into one queue, hand batches to the coordination Agent,
//! ask the user in the chat of the bot that owns each session, deliver webhooks from a
//! local outbox, hold the requests sessions wait on for the Agent, and give sessions no bot
//! started to the primary bot until it hands them to another or stops following them.
use super::{Access, Changed, Runtime};
use crate::clock::now_ms;
use crate::{
    config,
    manager::{self, CursorInstance, ManagerConfig},
    model::{EventBody, Subject},
    net,
    providers::Provider,
    store::{
        admission::AdmissionRejected,
        triage::{BudgetStatus, TriageAdmission},
    },
    triage::{
        self, AnswerAuthority, AnswerResponse, Delivery, HeldAnswerControl, HeldAnswerRequest,
        HookEvent, HookRequest, OutboxEntry, RequestKind, SealedAgentEvent, TriageConfig,
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

mod commands;

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

fn public_https(url: &str) -> bool {
    reqwest::Url::parse(url)
        .is_ok_and(|url| url.scheme() == "https" && net::mesh::public_host(&url))
}

/// Where a webhook's reader answers: `public_url` when it is public HTTPS, else the default
/// bootstrap node's gateway among the mesh's verified `gateways`, which hands each decision to
/// this Node by device ID. The answer carries the decision token, so no other mesh gateway,
/// which anyone may announce, ever receives it.
fn decision_url(public_url: &str, gateways: &[(String, String)]) -> Result<String> {
    let anchor = net::mesh::default_bootstrap_id()?;
    let gateway = gateways.iter().find(|(device, _)| *device == anchor);
    let base = match gateway {
        Some((_, url)) if !public_https(public_url) => url,
        _ => public_url,
    };
    Ok(format!("{}/api/decision", base.trim_end_matches('/')))
}

pub(crate) struct TriageState {
    config: std::sync::Mutex<TriageConfig>,
    /// Requests sessions hold until they are answered, by the Node's own request ID. An App
    /// Server request lives only as long as the connection that carried it.
    requests: Arc<std::sync::Mutex<HashMap<String, HeldRequest>>>,
    /// Answers given in this process: answering again returns the first answer.
    answered: std::sync::Mutex<HashMap<String, Value>>,
    wake: tokio::sync::Notify,
    outbox: tokio::sync::Notify,
    /// Metadata stays together until a completed thread's last answer has been consumed.
    threads: Arc<std::sync::Mutex<HashMap<String, ThreadMetadata>>>,
    started_ms: i64,
    pub(super) turn: tokio::sync::Mutex<()>,
    active: std::sync::Mutex<Option<String>>,
    budget_warning: std::sync::Mutex<Option<BudgetStatus>>,
    mutations: tokio::sync::Mutex<()>,
}

#[derive(Default)]
struct ThreadMetadata {
    cwd: String,
    last_message: String,
    background: Option<bool>,
    completed: bool,
    pending: usize,
    touched_ms: i64,
}

impl ThreadMetadata {
    fn observe(&mut self, thread: &Value) {
        if let Some(cwd) = thread["cwd"]
            .as_str()
            .filter(|cwd| !cwd.is_empty() && cwd.len() <= 4096)
        {
            self.cwd = cwd.into();
        }
        if matches!(thread["source"], Value::String(_) | Value::Object(_)) {
            self.background = Some(!triage::interactive_codex(&thread["source"], None));
        }
        self.touched_ms = now_ms();
    }
}

fn trim_threads(threads: &mut HashMap<String, ThreadMetadata>) {
    while threads.len() > 1024 {
        let Some(oldest) = threads
            .iter()
            .filter(|(_, entry)| {
                entry.completed && entry.pending == 0 && entry.last_message.is_empty()
            })
            .min_by_key(|(_, entry)| entry.touched_ms)
            .map(|(id, _)| id.clone())
        else {
            break;
        };
        threads.remove(&oldest);
    }
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

fn hook_answer_response(event: &HookEvent, schema: Option<&HookRequest>) -> Result<AnswerResponse> {
    if let Some(schema) = schema {
        ensure!(
            schema.provider == event.agent
                && schema.session_id == event.session_id
                && schema.turn_id == event.turn_id
                && Some(schema.kind) == event.hold,
            "hook request metadata does not belong to this event"
        );
    }
    Ok(match (Provider::from(event.agent), event.hold) {
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
fn summary(event: &SealedAgentEvent) -> String {
    let detail = if event.observation().hold.is_some() {
        ""
    } else {
        event.observation().detail.as_str()
    };
    [event.observation().last_message.as_str(), detail]
        .into_iter()
        .flat_map(|text| text.lines())
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or_default()
        .chars()
        .take(120)
        .collect()
}

fn subject(event: &SealedAgentEvent, request_id: Option<String>) -> Subject {
    Subject {
        node: event.node().clone(),
        agent: Some(event.provider()),
        session_id: event.observation().session_id.clone(),
        cwd: event.observation().cwd.clone(),
        summary: summary(event),
        request_id,
    }
}

fn shared_event(event: &SealedAgentEvent, roots: &[PathBuf]) -> SealedAgentEvent {
    crate::logging::add_private_path(Path::new(&event.observation().cwd));
    let mut shared = event.clone();
    let cwd = roots
        .iter()
        .enumerate()
        .filter_map(|(index, root)| {
            let relative = Path::new(&event.observation().cwd)
                .strip_prefix(root)
                .ok()?;
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
    shared.scrub_local_context(
        cwd,
        crate::logging::scrub(&event.observation().last_message),
        crate::logging::scrub(&event.observation().detail),
    );
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
fn owner(
    store: &crate::store::Store,
    network: &str,
    event: &SealedAgentEvent,
) -> Result<Option<String>> {
    if let Some(owner) =
        store.session_owner(network, event.node(), &event.observation().session_id)?
    {
        return Ok(Some(owner));
    }
    let bound = match store.native_thread_conversation(&event.observation().session_id)? {
        Some(conversation) => Some(conversation),
        None => store
            .connection
            .query_row(
                "SELECT conversation_id FROM work_runs WHERE thread_id=? ORDER BY work_id LIMIT 1",
                [&event.observation().session_id],
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
fn own_turn(store: &crate::store::Store, event: &SealedAgentEvent) -> Result<bool> {
    let coordination = store
        .connection
        .query_row(
            "SELECT 1 FROM coordination_threads WHERE thread_id=?",
            [&event.observation().session_id],
            |_| Ok(()),
        )
        .optional()?
        .is_some();
    Ok(coordination
        || (event.request_id().is_none()
            && store
                .native_thread_conversation(&event.observation().session_id)?
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

fn dequeue(connection: &Connection, event: &SealedAgentEvent) -> Result<()> {
    connection.execute(
        "DELETE FROM triage_queue WHERE ts_ms=? AND event_id=?",
        params![event.observation().ts_ms.max(0), event.event_id()],
    )?;
    Ok(())
}

fn outbox_from(connection: &Connection, now: i64) -> Result<Vec<OutboxEntry>> {
    let mut statement = connection.prepare(
        "SELECT event_id,body,state,attempts,next_ms FROM triage_outbox WHERE state='pending' AND next_ms<=? ORDER BY next_ms,event_id",
    )?;
    let rows = statement.query_map([now], |r| {
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

fn handover_outcome(
    node: &str,
    from: &str,
    session: &str,
    seen: &str,
    target: Option<&str>,
    reply: Value,
) -> Result<crate::tools::ToolOutcome> {
    use crate::tools::{Execution, uncertain};
    #[derive(serde::Deserialize)]
    struct HandoverReceipt {
        node: String,
        session_id: String,
        conversation_id: String,
        muted: bool,
        changed: bool,
        version: String,
    }
    let receipt: HandoverReceipt = serde_json::from_value(reply.clone())
        .context("invalid session handover receipt")
        .map_err(uncertain)?;
    let valid_version = receipt.version.len() == 16
        && receipt
            .version
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase());
    if !valid_version
        || (receipt.changed && receipt.version == seen)
        || receipt.node != node
        || receipt.session_id != session
        || receipt.conversation_id != target.unwrap_or(from)
        || receipt.muted != target.is_none()
    {
        return Err(uncertain(anyhow::anyhow!(
            "session handover receipt does not match the requested handover"
        )));
    }
    let effect = if receipt.changed {
        Execution::Applied
    } else {
        Execution::NotStarted
    };
    Ok(effect.result(reply))
}

impl Runtime {
    pub(crate) async fn network_id(&self) -> Result<String> {
        Ok(self.membership_state().await?.genesis.network_id)
    }

    /// Takes one event from a hook, a scan or the App Server. Test events are acknowledged and
    /// dropped; so is everything while no coordination Agent is configured to judge it, and
    /// everything of a session its bot stopped following. A session no bot owns, or whose bot
    /// was deleted, goes to the primary bot first; without a bot its events drop. An event ID
    /// seen before is a duplicate, whichever source sent it first.
    pub(crate) async fn ingest_agent_event(&self, event: HookEvent) -> Result<Value> {
        self.ingest_hook_event(event, None).await
    }

    pub(crate) async fn ingest_hook_event(
        &self,
        event: HookEvent,
        schema: Option<HookRequest>,
    ) -> Result<Value> {
        match event.hold {
            Some(kind) => self.hold_request(event, kind, schema).await,
            None => self.queue_event(event, None).await,
        }
    }

    /// A hook's event whose session waits for the answer: held like an App Server request, and
    /// answered to the hook waiting on `/api/hooks/wait`. An event that is not queued holds
    /// nothing, and neither does a second one for a request already held; their hooks leave
    /// the decision to the session.
    async fn hold_request(
        &self,
        event: HookEvent,
        kind: RequestKind,
        schema: Option<HookRequest>,
    ) -> Result<Value> {
        let response = hook_answer_response(&event, schema.as_ref())?;
        let request_id = SealedAgentEvent::id_for(
            &self.identity.device_id()?,
            event.agent,
            &event.session_id,
            &event.turn_id,
            event.event,
        );
        let now = now_ms();
        let request = HeldRequest {
            request_id: request_id.clone(),
            provider: event.agent.into(),
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
        let provider = event.agent;
        let queued = self.queue_event(event, Some(request_id.clone())).await;
        match queued {
            Ok(mut result) if result["status"] == "accepted" && result["test"] != true => {
                tracing::info!(event = "request.held", request = %request_id, provider = provider.name(), ?kind,
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

    async fn queue_event(&self, event: HookEvent, request_id: Option<String>) -> Result<Value> {
        crate::logging::add_private_path(Path::new(&event.cwd));
        let mut event = SealedAgentEvent::seal(
            event,
            &self.identity.device_id()?,
            config::UNNAMED_NODE,
            now_ms(),
        )?;
        if let Some(request_id) = request_id {
            event.bind_request(request_id);
        }
        if event.observation().test {
            tracing::info!(
                event = "triage.test_received",
                event_id = event.event_id(),
                "triage test event received"
            );
            return Ok(json!({"status":"accepted","event_id":event.event_id(),"test":true}));
        }
        let network = self.network_id().await?;
        if self.manager().is_none() {
            return Ok(json!({"status":"dropped","event_id":event.event_id()}));
        }
        let identity = self.identity.clone();
        let roots = self.settings_snapshot().roots;
        let threads = self.triage.threads.clone();
        let requests = self.triage.requests.clone();
        let actor_network = network.clone();
        let (result, claimed) = self
            .store
            .call("triage.queue_event", move |store| {
                if own_turn(store, &event)? {
                    return Ok((json!({"status":"own","event_id":event.event_id()}), None));
                }
                if store.session_muted(
                    &actor_network,
                    event.node(),
                    &event.observation().session_id,
                )? {
                    return Ok((json!({"status":"muted","event_id":event.event_id()}), None));
                }
                let (scope, claimed) = match owner(store, &actor_network, &event)? {
                    Some(scope) => (Some(scope), None),
                    None => Self::claim_session(
                        store,
                        &identity,
                        &roots,
                        &threads,
                        &actor_network,
                        &event,
                    )?,
                };
                let Some(scope) = scope else {
                    return Ok((
                        json!({"status":"foreign","event_id":event.event_id()}),
                        None,
                    ));
                };
                if let Some(id) = event.request_id() {
                    let epoch = store
                        .conversation(&actor_network, &scope)?
                        .map(|conversation| conversation.owner_epoch);
                    if let Some(request) = lock(&requests).get_mut(id) {
                        request.owner_epoch = epoch;
                    }
                }
                let event_id = event.event_id().clone();
                let (provider, kind, origin) = (
                    event.provider().name(),
                    event.observation().event,
                    event.observation().source,
                );
                let session = event.observation().session_id.clone();
                let status = if store.queue_triage_event(event, now_ms())? {
                    tracing::info!(
                        event = "triage.queued",
                        conversation_id = scope,
                        thread_id = session,
                        event_id,
                        provider,
                        kind = ?kind,
                        origin = ?origin,
                        "session event queued for triage"
                    );
                    "accepted"
                } else {
                    "duplicate"
                };
                Ok((
                    json!({"status":status,"event_id":event_id}),
                    claimed.map(|hash| (scope, hash)),
                ))
            })
            .await?;
        if let Some((scope, hash)) = claimed {
            self.publish_changed(&network, &scope, Changed::Event(&hash));
        }
        Ok(result)
    }

    /// Gives a session this device runs and no bot owns to the primary bot, written once.
    /// Returns that bot's conversation and the optional attribution event hash.
    fn claim_session(
        store: &mut crate::store::Store,
        identity: &crate::crypto::LocalIdentity,
        roots: &[PathBuf],
        threads: &std::sync::Mutex<HashMap<String, ThreadMetadata>>,
        network: &str,
        event: &SealedAgentEvent,
    ) -> Result<(Option<String>, Option<String>)> {
        let background = lock(threads)
            .get(&event.observation().session_id)
            .and_then(|thread| thread.background);
        if event.observation().session_id == NO_THREAD
            || background == Some(true)
            || (event.observation().source == triage::Source::AppServer && background.is_none())
        {
            return Ok((None, None));
        }
        let Some(primary) = store.primary_bot(network)? else {
            return Ok((None, None));
        };
        let written = store.claim_session(
            identity,
            network,
            &primary,
            &subject(&shared_event(event, roots), None),
            now_ms(),
        )?;
        if written.is_some() {
            tracing::info!(
                event = "session.attributed",
                conversation_id = primary,
                thread_id = event.observation().session_id,
                "a session no bot started went to the primary bot"
            );
        }
        Ok((Some(primary), written.map(|event| event.hash)))
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
            let request_id = request_id.clone();
            self.store.call("triage.request_question", move |store| {
                store.request_question(&network, &node, &request_id)
            }).await
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
            .map(|thread| thread.cwd.clone())
            .unwrap_or_default();
        if cwd.is_empty()
            && let Some(thread) = &handed.thread_id
        {
            let thread = thread.clone();
            cwd = self
                .store
                .call("triage.work_cwd_on_thread", move |store| {
                    store.work_cwd_on_thread(&thread)
                })
                .await
                .ok()
                .flatten()
                .unwrap_or_default();
        }
        if cwd.is_empty() {
            cwd = handed.params["cwd"].as_str().unwrap_or_default().to_owned();
        }
        let event = HookEvent {
            source: triage::Source::AppServer,
            agent: Provider::Codex.into(),
            event: triage::Kind::NeedsInput,
            session_id,
            turn_id: format!("request:{request_id}"),
            cwd,
            detail: describe(&handed.method, &handed.params),
            ..Default::default()
        };
        let queued = self.queue_event(event, Some(request_id.clone())).await;
        let waiting = match &queued {
            Ok(result) if result["status"] == "accepted" => true,
            // Its event still waits for the Agent from the earlier connection.
            Ok(result) if result["status"] == "duplicate" => {
                let event_id = result["event_id"].as_str().unwrap_or_default().to_owned();
                let actor_request_id = request_id.clone();
                let attached = self
                    .store
                    .call("triage.attach_request", move |store| {
                        store.attach_triage_request(&event_id, &actor_request_id)
                    })
                    .await;
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
        let request_id = request_id.to_owned();
        let answered = by != "fallback";
        self.store
            .call("triage.finish_request", move |store| {
                store.finish_triage_request(&request_id, answered)
            })
            .await?;
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
                let question = question.to_owned();
                let answered = self
                    .store
                    .call("triage.expire_question", move |store| {
                        Ok(store
                            .question(&network, &question)?
                            .is_some_and(|question| !question["answer"].is_null()))
                    })
                    .await?;
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
                let network = network.clone();
                let question = question.to_owned();
                self.store
                    .call("triage.queue_live_answer", move |store| {
                        let Some(question_body) = store.question(&network, &question)? else {
                            return Ok(false);
                        };
                        if question_body["answer"].is_null() {
                            return Ok(false);
                        }
                        let scope = question_body["conversation_id"]
                            .as_str()
                            .context("question has no conversation")?;
                        if store
                            .held_answer_receipt(
                                &network,
                                scope,
                                &crate::store::questions::answer_id(&question),
                            )?
                            .is_some()
                        {
                            return Ok(false);
                        }
                        store.queue_answer_control(
                            &network,
                            &question,
                            &binding,
                            AnswerAuthority::Signed,
                            now_ms(),
                        )
                    })
                    .await
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
        let requests = self.triage.requests.clone();
        let network = network.to_owned();
        let conversation = conversation.to_owned();
        let message_id = message_id.to_owned();
        let receipt =
            self.store
                .call("triage.accept_answer_delivery", move |store| {
                    let Some(message) = store.message(&network, &conversation, &message_id)? else {
                        return Ok(None);
                    };
                    let Some(question_id) = &message.answers else {
                        return Ok(None);
                    };
                    let question = store
                        .question(&network, question_id)?
                        .context("answer question is missing")?;
                    let Some(request_id) = question["about"]["request_id"].as_str() else {
                        return Ok(None);
                    };
                    ensure!(
                        question["about"]["node"] == node,
                        AdmissionRejected::new("this Node does not hold the answer's request")
                    );
                    if let Some(receipt) =
                        store.held_answer_receipt(&network, &conversation, &message_id)?
                    {
                        return Ok(Some(receipt));
                    }
                    let request = lock(&requests).get(request_id).cloned().context(
                        AdmissionRejected::new(
                            "the original request is no longer waiting on this Node",
                        ),
                    )?;
                    ensure!(
                        request.question_id.as_deref() == Some(question_id),
                        AdmissionRejected::new("the held request belongs to another question")
                    );
                    store.queue_answer_control(
                        &network,
                        question_id,
                        &request.answer_binding(&node),
                        AnswerAuthority::Signed,
                        now_ms(),
                    )?;
                    let receipt = store
                        .held_answer_receipt(&network, &conversation, &message_id)?
                        .context("answer control receipt was not saved")?;
                    Ok(Some(receipt))
                })
                .await?;
        if receipt.is_some() {
            self.triage.wake.notify_one();
        }
        Ok(receipt)
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
        fn active_group(
            store: &crate::store::Store,
            control: &HeldAnswerControl,
        ) -> Result<Vec<crate::work::WorkRecord>> {
            let request = &control.request;
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
        }
        let (manager_attempt, managed, runs) = {
            let request = request.clone();
            self.store.call("triage.answer_manager_group", move |store| {
                let manager: Option<String> = store.connection.query_row(
                    "SELECT attempt_id FROM manager_turns WHERE thread_id=?1 AND (?2='' OR turn_id=?2) ORDER BY rowid DESC LIMIT 1",
                    params![request.session_id, request.turn_id], |row| row.get(0),
                ).optional()?;
                Ok((
                        manager,
                        store
                            .native_thread_conversation(&request.session_id)?
                            .is_some(),
                        store.work_records_on_thread(&request.session_id)?,
                ))
            }).await?
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
            let control = control.clone();
            self.store
                .call("triage.answer_active_group", move |store| {
                    active_group(store, &control)
                })
                .await?
        };
        let work_ids: Vec<_> = active.iter().map(|record| record.work_id.clone()).collect();
        let work_id = &work_ids[0];
        let mut leases = Vec::new();
        let parent = {
            let control = control.clone();
            let work_id = work_id.clone();
            self.store
                .call("triage.answer_parent", move |store| {
                    let work = store
                        .work(&control.network, &work_id)?
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
                        Ok(Some((parent.work_id, attempt, provider)))
                    } else {
                        Ok(None)
                    }
                })
                .await?
        };
        // Parent first, then stable Work order, matching cancellation's lock order.
        if let Some((_, attempt, provider)) = &parent {
            leases.push(self.manager_input_lease(attempt, *provider).await?);
        }
        for id in &work_ids {
            leases.push(self.work_input_lock(id).await.read_owned().await);
        }
        let cwd = {
            let control = control.clone();
            let work_id = work_id.clone();
            self.store
                .call("triage.answer_cwd", move |store| {
                    let work = store
                        .work(&control.network, &work_id)?
                        .context("the request's work is missing")?;
                    Self::validate_work_assignment_roots(store, &control.network, &work.trigger)?;
                    Ok(work.cwd)
                })
                .await?
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
        let allowed = self
            .providers()
            .switches
            .execution
            .contains(&request.provider);
        let permission_epoch = self
            .local_permission_epoch
            .load(std::sync::atomic::Ordering::Acquire);
        let actor_control = control.clone();
        self.store
            .call("triage.answer_permission", move |store| {
                let control = &actor_control;
                let request = &control.request;
                store.authorize_node(&control.network, &request.node)?;
                let active = active_group(store, control)?;
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
                            && !store.peer_cancelled(
                                &control.network,
                                &record.conversation,
                                work_id
                            )?,
                        "the request's work was stopped or changed provider"
                    );
                    ensure!(
                        store.work_native_write_allowed(&control.network, work_id)?,
                        "the request's work has no current authority for native submission"
                    );
                    if let Some(handle) = record.state.handle() {
                        ensure!(
                            (request.turn_id.is_empty() || handle.turn_id == request.turn_id)
                                && execution.as_ref().is_none_or(|execution| handle.thread_id
                                    == execution.thread_id
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
                            && allowed
                            && permission_epoch == request.permission_epoch,
                        "the request's execution permission was revoked"
                    );
                    Self::validate_work_assignment_roots(store, &control.network, &work.trigger)?;
                    ensure!(
                        (work.cwd.is_empty()
                            || PathBuf::from(&work.cwd).canonicalize()? == actual_cwd)
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
                Ok(())
            })
            .await?;
        ensure!(
            self.local_permission_epoch
                .load(std::sync::atomic::Ordering::Acquire)
                == request.permission_epoch
                && self
                    .providers()
                    .switches
                    .execution
                    .contains(&request.provider),
            "the request's execution permission was revoked"
        );
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
        self.store.call("triage.drop_requests", move |store| {
            for id in lost {
                tracing::info!(event = "request.lost", request = %id, "App Server request lost with its connection");
                store.finish_triage_request(&id, false)?;
            }
            Ok(())
        }).await
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
        let identity = self.identity.clone();
        let actor_network = network.clone();
        let closed = self
            .store
            .call("triage.close_lost_questions", move |store| {
                store.close_lost_questions(&identity, &actor_network, &held, now_ms())
            })
            .await?;
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
        let entry = threads.entry(thread.clone()).or_default();
        entry.touched_ms = now_ms();
        match event["method"].as_str().unwrap_or_default() {
            "thread/started" => {
                entry.observe(&params["thread"]);
                entry.completed = false;
            }
            "turn/started" => entry.completed = false,
            "item/completed" if params["item"]["type"] == "agentMessage" => {
                entry.last_message = params["item"]["text"].as_str().unwrap_or_default().into();
                entry.completed = false;
            }
            "turn/completed" => {
                let ended = vibeface_bridge::turn::turn_snapshot(&params["turn"]);
                let status = ended
                    .status
                    .as_ref()
                    .map_or("completed", |status| status.as_str());
                let classification_unknown = entry.background.is_none();
                let mut event = HookEvent {
                    source: triage::Source::AppServer,
                    agent: Provider::Codex.into(),
                    event: triage::Kind::TurnEnd,
                    status: Some(status.into()),
                    session_id: thread.clone(),
                    turn_id: ended.id.unwrap_or_default(),
                    cwd: entry.cwd.clone(),
                    last_message: std::mem::take(&mut entry.last_message),
                    detail: ended.error.map(|error| error.detail).unwrap_or_default(),
                    ..Default::default()
                };
                entry.completed = true;
                entry.pending += 1;
                let node = self.clone();
                if let Err(error) = self.background_tasks.spawn(async move {
                    // Another CLI connection may have created the thread before this observer joined.
                    if (event.cwd.is_empty() || classification_unknown)
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
                        let mut threads = lock(&node.triage.threads);
                        let metadata = threads.entry(event.session_id.clone()).or_default();
                        metadata.observe(&reply["thread"]);
                        event.cwd.clone_from(&metadata.cwd);
                    }
                    let session_id = event.session_id.clone();
                    if let Err(error) = node.ingest_agent_event(event).await {
                        tracing::debug!(%error, "App Server turn was not triaged");
                    }
                    let mut threads = lock(&node.triage.threads);
                    if let Some(metadata) = threads.get_mut(&session_id) {
                        metadata.pending -= 1;
                    }
                    trim_threads(&mut threads);
                }) {
                    entry.pending -= 1;
                    tracing::debug!(%error, "App Server triage skipped while the Node is shutting down");
                }
            }
            _ => {}
        }
        trim_threads(&mut threads);
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

    async fn pending_events(&self) -> Result<Vec<SealedAgentEvent>> {
        self.store
            .call("triage.pending_events", |store| store.triage_events(None))
            .await
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
        config
            .instructions
            .push_str(triage::OBSERVATION_INSTRUCTIONS);
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
            self.store
                .call("triage.record_coordination_thread", move |store| {
                    store.connection.execute(
                        "INSERT INTO coordination_threads(thread_id) VALUES(?)
                     ON CONFLICT(thread_id) DO NOTHING",
                        [thread],
                    )?;
                    Ok(())
                })
                .await?;
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
        let answering = self
            .store
            .call("triage.pending_answer_controls", |store| {
                store.has_pending_answer_controls()
            })
            .await?;
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
            let network = network.clone();
            let boot = self.boot.clone();
            let limit = self.config.daily_decision_limit;
            self.store
                .call("triage.admit", move |store| {
                    store.admit_triage(&network, &boot, BATCH, limit, now_ms())
                })
                .await?
        };
        let attempt = match admission {
            TriageAdmission::Empty => return Ok(()),
            TriageAdmission::Paused(status) => {
                let pending = self.pending_events().await?;
                let mut requests = lock(&self.triage.requests);
                for event in &pending {
                    if let Some(id) = event.request_id()
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
            return self
                .store
                .call("triage.finish_answer_control", move |store| {
                    store.finish_triage(attempt, &result)?;
                    Ok(result)
                })
                .await?;
        }
        let fresh = attempt.events();
        for event in fresh {
            if let Some(id) = event.request_id()
                && let Some(request) = lock(&self.triage.requests).get_mut(id)
            {
                request.fallback_reason = None;
            }
        }
        let turn = async {
            let network = network.clone();
            let events = fresh.to_vec();
            let roots = self.settings_snapshot().roots;
            let (offered, bots) = self
                .store
                .call("triage.prepare_offer", move |store| {
                    let mut offered = Vec::new();
                    let mut bots = Vec::new();
                    let mut seen = HashSet::new();
                    for event in &events {
                        let owner = owner(store, &network, event)?;
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
                            shared_event(event, &roots),
                            json!({"conversation_id":owner,"name":name}),
                        ));
                    }
                    Ok((offered, bots))
                })
                .await
                .map_err(manager::ManagerError::not_started)?;
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
        self.store
            .call("triage.finish", move |store| {
                store.finish_triage(attempt, &turn)?;
                Ok(turn)
            })
            .await?
    }

    async fn settle_offered(&self, offered: &[SealedAgentEvent]) -> Result<()> {
        for event in offered {
            if let Some(request_id) = event.request_id() {
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
        let network = self
            .store
            .read("triage.status_network", |store| store.network())
            .await?
            .unwrap_or_default();
        let active = lock(&self.triage.active).clone();
        Ok(serde_json::to_value({
            let boot = self.boot.clone();
            let limit = self.config.daily_decision_limit;
            self.store
                .read("triage.status", move |store| {
                    store.triage_status(&network, &boot, active.as_deref(), limit, now_ms())
                })
                .await?
        })?)
    }

    pub(super) async fn respond_unclaimed_request(
        &self,
        request_id: &str,
        result: Value,
    ) -> Result<Value> {
        let _mutations = self.triage.mutations.lock().await;
        if let Some(done) = lock(&self.triage.answered).get(request_id) {
            return Ok(done.clone());
        }
        ensure!(
            self.store
                .call("triage.unclaimed_events", |store| store.triage_events(None))
                .await?
                .iter()
                .any(|event| event.request_id().as_deref() == Some(request_id)),
            "request is not available to this triage attempt"
        );
        self.answer_request(request_id, Some(Ok(result)), "agent")
            .await
    }

    /// Asks in the chat of the bot that owns the event's session, the primary bot's when its
    /// bot was deleted after the event came. An unanswered question about the same session and
    /// request stands instead of a new one; a request it names waits for the answer.
    async fn raise(
        &self,
        network: &str,
        event: &SealedAgentEvent,
        mut question: EventBody,
    ) -> Result<Value> {
        let identity = self.identity.clone();
        let roots = self.settings_snapshot().roots;
        let threads = self.triage.threads.clone();
        let requests = self.triage.requests.clone();
        let actor_network = network.to_owned();
        let actor_event = event.clone();
        let (asked, hash, owner, claimed) = self
            .store
            .call("triage.raise", move |store| {
                let network = actor_network.as_str();
                let event = &actor_event;
                let (owner, claimed) = match owner(store, network, event)? {
                    Some(owner) => (owner, None),
                    None => {
                        let (owner, claimed) = Self::claim_session(
                            store, &identity, &roots, &threads, network, event,
                        )?;
                        (owner.context("no bot owns this session")?, claimed)
                    }
                };
                if event.node().as_str() == identity.device_id()?
                    && let EventBody::QuestionAsked { target, about, .. } = &mut question
                {
                    let native_turn =
                        if matches!(&event.observation().source, triage::Source::AppServer)
                            && event.request_id().is_some()
                        {
                            event.request_id().as_deref().and_then(|id| {
                                lock(&requests)
                                    .get(id)
                                    .filter(|request| {
                                        request.request_id == id
                                            && about.request_id.as_deref() == Some(id)
                                            && request.provider == event.provider()
                                            && request.session_id == event.observation().session_id
                                            && !request.session_turn.is_empty()
                                            && matches!(&request.reply, Reply::AppServer { .. })
                                    })
                                    .map(|request| request.session_turn.clone())
                            })
                        } else {
                            Some(event.observation().turn_id.clone())
                        };
                    *target = if let Some(native_turn) = native_turn {
                        match store.native_input_target(
                            network,
                            &owner,
                            &event.observation().session_id,
                            &native_turn,
                        )? {
                            Some(target) => Some(target),
                            None => {
                                store.attributed_session_input_target(network, &owner, about)?
                            }
                        }
                    } else {
                        None
                    };
                }
                let (asked, written) = store.ask(&identity, network, &owner, question, now_ms())?;
                Ok((asked, written.map(|event| event.hash), owner, claimed))
            })
            .await?;
        if let Some(hash) = claimed {
            self.publish_changed(network, &owner, Changed::Event(&hash));
        }
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
                thread_id = event.observation().session_id,
                question_id,
                "question asked in the bot chat"
            );
            self.publish_changed(network, &owner, Changed::Event(hash));
            sent = self.send_question(&asked, &event.observation().cwd).await?;
        }
        Ok(
            json!({"event_id":event.event_id(),"question_id":question_id,"conversation_id":owner,
            "asked":hash.is_some(),"queued_webhook":sent}),
        )
    }

    async fn decision_url(&self) -> Result<String> {
        let gateways = match self.net().ok().and_then(|net| net.mesh()) {
            Some(mesh) if !public_https(&self.config.public_url) => {
                let anchor = net::mesh::default_bootstrap_id()?;
                let url = mesh.gateway_of(&anchor).await;
                url.map(|url| (anchor, url)).into_iter().collect()
            }
            _ => vec![],
        };
        decision_url(&self.config.public_url, &gateways)
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
            "answer":{"url":self.decision_url().await?, "device_id":self.identity.device_id()?,
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
        let answered: Vec<(String, String, String)> = self.store.call("triage.answered_outbox", move |store| {
            let mut statement = store.connection.prepare(
                "SELECT q.question_id,m.text,'answered-'||q.question_id||'-'||substr(m.event_hash,1,16) FROM triage_outbox o
                 JOIN questions q ON q.network_id=?1 AND q.question_id=o.event_id
                 JOIN messages m ON m.network_id=q.network_id AND m.conversation_id=q.conversation_id AND m.answers=q.question_id
                 WHERE NOT EXISTS(SELECT 1 FROM triage_outbox a WHERE a.event_id='answered-'||q.question_id||'-'||substr(m.event_hash,1,16))",
            )?;
            Ok(statement
                    .query_map([&network], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
                .collect::<rusqlite::Result<_>>()?)
        }).await?;
        for (question_id, text, event_id) in answered {
            {
                let question_id = question_id.clone();
                self.store.call("triage.prune_answer_outbox", move |store| {
                    store.connection.execute(
                        "DELETE FROM triage_outbox WHERE event_id LIKE 'answered-'||?||'-%' AND state=?",
                        params![question_id, name(Delivery::Pending)?],
                    )?;
                    Ok(())
                }).await?;
            }
            let body = json!({"schema_version":triage::SCHEMA_VERSION,"type":"answered",
                "event_id":event_id,"question_id":question_id,"text":text});
            self.queue_webhook(&event_id, &body).await?;
        }
        Ok(())
    }

    /// One outbox entry per event ID, however often it is queued.
    async fn queue_webhook(&self, event_id: &str, body: &Value) -> Result<()> {
        let entry = OutboxEntry::new(event_id, body.to_string(), now_ms());
        self.store
            .call("triage.queue_webhook", move |store| {
                store.connection.execute(
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
                Ok(())
            })
            .await?;
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
    ) -> Result<crate::tools::ToolOutcome> {
        use crate::tools::{Execution, uncertain};
        if node != self.identity.device_id()? {
            let reply = self
                .peer_request_with_send_boundary(
                    network,
                    node,
                    json!({"type":"hand_over_session","session_id":session,"version":seen,"from":from,"conversation_id":target}),
                    uncertain,
                )
                .await?;
            return handover_outcome(node, from, session, seen, target, reply);
        }
        let (before, version, event) = {
            let identity = self.identity.clone();
            let network = network.to_owned();
            let from = from.to_owned();
            let session = session.to_owned();
            let seen = seen.to_owned();
            let target = target.map(str::to_owned);
            self.store
                .tool_transaction("triage.hand_over_session", move |store| {
                    store.hand_over_session(
                        &identity,
                        &network,
                        &from,
                        &session,
                        &seen,
                        target.as_deref(),
                        now_ms(),
                    )
                })
                .await?
        };
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
        let effect = if event.is_some() {
            Execution::Applied
        } else {
            Execution::NotStarted
        };
        Ok(effect.result(
            json!({"node":node,"session_id":session,"conversation_id":owner,"muted":target.is_none(),
            "version":version,"changed":event.is_some()}),
        ))
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
        let entries = self
            .store
            .call("triage.due_webhooks", |store| {
                outbox_from(&store.connection, now_ms())
            })
            .await?;
        for mut entry in entries {
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
            self.store
                .call("triage.record_webhook", move |store| {
                    store.connection.execute(
                        "UPDATE triage_outbox SET state=?,attempts=?,next_ms=? WHERE event_id=?",
                        params![
                            name(entry.state)?,
                            entry.attempts,
                            entry.next_ms,
                            entry.event_id
                        ],
                    )?;
                    Ok(())
                })
                .await?;
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
                    .and_then(|bytes| serde_json::from_slice::<HookEvent>(&bytes).ok());
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
        self.store.call("triage.prune_seen", |store| {
            let cutoff = now_ms() - SEEN_TTL_MS;
            store
                .connection
                .execute("DELETE FROM triage_seen WHERE ts_ms<? AND event_id NOT LIKE 'answer:%' AND NOT EXISTS(SELECT 1 FROM triage_queue q WHERE q.event_id=triage_seen.event_id)", [cutoff])?;
            store.connection.execute(
                "DELETE FROM triage_handled WHERE ts_ms<? AND event_id NOT LIKE 'answer:%'",
                [cutoff],
            )?;
            Ok(())
        }).await?;
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
            let mark: Option<usize> = {
                let rollout = rollout.clone();
                self.store
                    .call("triage.scan_mark", move |store| {
                        Ok(store
                            .connection
                            .query_row(
                                "SELECT scanned_to FROM triage_scans WHERE rollout=?",
                                [&rollout],
                                |r| r.get(0),
                            )
                            .optional()?)
                    })
                    .await?
            };
            if let Some(mark) = mark
                && std::fs::metadata(&path).is_ok_and(|meta| meta.len() == mark as u64)
            {
                continue;
            }
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
            self.store
                .call("triage.record_scan", move |store| {
                    store.connection.execute(
                        "INSERT INTO triage_scans(rollout,scanned_to) VALUES(?,?)
                     ON CONFLICT(rollout) DO UPDATE SET scanned_to=excluded.scanned_to",
                        params![rollout, end],
                    )?;
                    Ok(())
                })
                .await?;
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
        let outbox = self
            .store
            .read("triage.outbox_summary", |store| {
                let mut outbox = json!({"pending":0,"delivered":0,"dead":0});
                let mut statement = store
                    .connection
                    .prepare("SELECT state,COUNT(*) FROM triage_outbox GROUP BY state")?;
                let rows = statement
                    .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, u64>(1)?)))?;
                for row in rows {
                    let (state, count) = row?;
                    outbox[state] = json!(count);
                }
                Ok(outbox)
            })
            .await?;
        Ok(json!({
            "default_outlet":config.default_outlet, "projects":config.projects,
            "webhook_url":config.webhook_url, "scan_codex_sessions":config.scan_codex_sessions,
            "codex_sessions":config.codex_sessions,
            "webhook_secret_set":self.webhook_secret()?.is_some(),
            "decision_url":self.decision_url().await?, "device_id":self.identity.device_id()?,
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
        let network = access.network_id().to_owned();
        let questions = self
            .store
            .read("triage.questions", move |store| {
                store.unanswered_questions(&network)
            })
            .await?;
        Ok(json!({"questions":questions}))
    }

    /// The user's answer, signed by their device, to a question in any chat.
    pub(super) async fn answer_question(&self, access: &Access, input: &Value) -> Result<Value> {
        ensure!(access.is_user(), "only the user answers questions");
        let answered = self
            .post_answer(
                access.network_id(),
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
        let question_body = {
            let network = network.to_owned();
            let question = question.to_owned();
            self.store
                .call("triage.answer_question", move |store| {
                    store.question(&network, &question)
                })
                .await?
        }
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
        let answered = {
            let identity = self.identity.clone();
            let network = network.to_owned();
            let question = question.to_owned();
            let text = text.to_owned();
            self.store
                .call("triage.accept_answer", move |store| {
                    store.accept_answer_with_control(
                        &identity,
                        &network,
                        &question,
                        &text,
                        attestation,
                        now_ms(),
                        binding.as_ref().map(|request| (request, authority)),
                    )
                })
                .await?
        };
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
        // A public node forwards to any device it can reach; other Nodes only to their own
        // network's Nodes. The target alone checks the token.
        if !self.config.forwarding.any {
            let network = self.network_id().await?;
            let device_id = input.device_id.clone();
            self.store
                .call("triage.authorize_decision_target", move |store| {
                    store.authorize_node(&network, &device_id)
                })
                .await?;
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
        let question = {
            let network = network.clone();
            let question_id = input.question_id.clone();
            self.store
                .call("triage.local_decision_question", move |store| {
                    store.question(&network, &question_id)
                })
                .await?
        }
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
mod test_support;
#[cfg(test)]
mod tests;
