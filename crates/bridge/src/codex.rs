use crate::{
    failure::{self, ExecFailure, FailureKind},
    methods::{self, Category},
    turn::{TurnStatus, turn_snapshot},
};
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque},
    fmt,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};
use tokio::{
    net::UnixStream,
    sync::{RwLock, mpsc, oneshot, watch},
    time::{Instant, timeout},
};
use tokio_tungstenite::{WebSocketStream, client_async, tungstenite::Message};
use vibeface_protocol::{RpcId, now_ms};

const RPC_TIMEOUT: Duration = Duration::from_secs(60);

#[derive(Debug)]
pub struct CodexError {
    pub uncertain: bool,
    /// The App Server itself answered with this error, as for a thread it does not know; a lost
    /// connection or timeout is not an answer.
    pub answered: bool,
    pub message: String,
}

impl fmt::Display for CodexError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for CodexError {}

impl CodexError {
    /// Why it failed: Codex's answer by its wording, else a refusal; no answer is a transport
    /// failure.
    pub fn failure(&self) -> ExecFailure {
        let otherwise = if self.answered {
            FailureKind::Rejected
        } else {
            FailureKind::Transport
        };
        ExecFailure::upstream(self.message.clone(), otherwise)
    }
}

/// Whether Codex refused because another process is writing the thread (Codex keeps one
/// writer per thread across processes). Nothing was sent, and retrying does not help until that
/// process lets go.
fn writer_busy(message: &str) -> bool {
    ExecFailure::upstream(message, FailureKind::Unknown).kind == FailureKind::WriterBusy
}

fn server_error(error: &Value) -> String {
    let detail = error["message"]
        .as_str()
        .map_or_else(|| error.to_string(), str::to_owned);
    if writer_busy(&detail) {
        format!(
            "another Codex process is writing this thread ({detail}); nothing was sent, close it there or wait"
        )
    } else {
        format!("Codex rejected request: {error}")
    }
}

fn rejected(message: impl Into<String>) -> CodexError {
    CodexError {
        uncertain: false,
        answered: false,
        message: message.into(),
    }
}

fn answered(message: impl Into<String>) -> CodexError {
    CodexError {
        uncertain: false,
        answered: true,
        message: message.into(),
    }
}

/// What the connection to the App Server reports to the Node, in the order it happened.
#[derive(Debug, Clone, PartialEq)]
pub enum ExecEvent {
    /// A connection completed its handshake.
    Connected,
    /// The connection ended, or the next one did not come up; its requests are gone.
    Disconnected,
    /// Every observed thread was read once on this connection, so any request the App Server
    /// asks again on them has arrived.
    Observed,
    /// A server request on one of the Node's threads, or on none: the Node answers it. The same
    /// message also follows as `Message`.
    Request(ServerRequest),
    /// A notification or server request as the App Server sent it.
    Message(Value),
}

#[derive(Debug, Clone, PartialEq)]
pub struct ServerRequest {
    pub id: RpcId,
    pub method: String,
    pub thread_id: Option<String>,
    pub params: Value,
}

#[derive(Clone)]
pub struct CodexClient {
    commands: mpsc::UnboundedSender<Command>,
    cache: Arc<RwLock<Cache>>,
    observed_threads: watch::Sender<BTreeSet<String>>,
}

impl CodexClient {
    /// Connects to whatever socket `endpoint` names and follows it when it changes; `None` stays
    /// disconnected. Setting the same path again cuts a reconnect backoff short.
    pub fn start(
        endpoint: watch::Receiver<Option<PathBuf>>,
        events: mpsc::UnboundedSender<ExecEvent>,
    ) -> Self {
        let (commands, receiver) = mpsc::unbounded_channel();
        let cache = Arc::new(RwLock::new(Cache::default()));
        let (observed_threads, observed) = watch::channel(BTreeSet::new());
        tokio::spawn(connection_loop(
            endpoint,
            receiver,
            observed,
            cache.clone(),
            events,
        ));
        Self {
            commands,
            cache,
            observed_threads,
        }
    }

    pub async fn request(&self, method: &str, params: Value) -> Result<Value, CodexError> {
        validate_request(method, &params)?;
        self.send(NativeCall {
            method: method.into(),
            params,
        })
        .await
    }

    async fn send(&self, call: NativeCall) -> Result<Value, CodexError> {
        let uncertain = call.uncertain();
        let (reply, response) = oneshot::channel();
        self.commands
            .send(Command::Call(Execute { call, reply }))
            .map_err(|_| rejected("Codex adapter is stopped"))?;
        response.await.unwrap_or_else(|_| {
            Err(CodexError {
                uncertain,
                answered: false,
                message: "Codex adapter stopped before returning a result".into(),
            })
        })
    }

    pub fn set_observed_threads(&self, threads: Vec<String>) -> Result<(), CodexError> {
        for thread in &threads {
            vibeface_protocol::validate_id(thread).map_err(rejected)?;
        }
        self.observed_threads
            .send_replace(threads.into_iter().collect());
        Ok(())
    }

    pub async fn snapshot(&self) -> Value {
        self.cache.read().await.snapshot()
    }

    /// What the web shows: connection, each thread's status, plan, reasoning summary and token
    /// usage, and rate limits. Commands, output, diffs and changed files stay out.
    pub async fn status(&self) -> Value {
        let cache = self.cache.read().await;
        let threads: Vec<Value> = cache
            .threads
            .iter()
            .map(|(id, thread)| {
                json!({"thread_id":id,"name":thread.name,"status":thread.status,"cwd":thread.cwd,
                    "plan":thread.plan,"reasoning":thread.reasoning,"token_usage":thread.token_usage})
            })
            .collect();
        json!({"connection":cache.connection,"threads":threads,"rate_limits":cache.rate_limits})
    }

    /// Answers a server request this connection received and still holds. Fails when it was
    /// answered already or the connection that carried it is gone.
    pub async fn respond(
        &self,
        id: &Value,
        result: Result<Value, Value>,
        decision: &str,
    ) -> Result<(), CodexError> {
        let id: RpcId = serde_json::from_value(id.clone())
            .map_err(|_| rejected("invalid server request ID"))?;
        let (reply, response) = oneshot::channel();
        self.commands
            .send(Command::Respond(Respond {
                id,
                result,
                decision: decision.into(),
                reply,
            }))
            .map_err(|_| rejected("Codex adapter is stopped"))?;
        response
            .await
            .unwrap_or_else(|_| Err(rejected("Codex adapter stopped before answering")))
    }
}

struct NativeCall {
    method: String,
    params: Value,
}

impl NativeCall {
    fn uncertain(&self) -> bool {
        methods::access(&self.method) != methods::Access::Read
    }
}

fn validate_request(method: &str, params: &Value) -> Result<(), CodexError> {
    if method.is_empty() || method.len() > 256 || method.chars().any(char::is_control) {
        return Err(rejected("invalid native method"));
    }
    if serde_json::to_vec(params)
        .map_err(|error| rejected(error.to_string()))?
        .len()
        > 1_048_576
    {
        return Err(rejected("native request is too large"));
    }
    Ok(())
}

struct Execute {
    call: NativeCall,
    reply: oneshot::Sender<Result<Value, CodexError>>,
}

struct Respond {
    id: RpcId,
    result: Result<Value, Value>,
    decision: String,
    reply: oneshot::Sender<Result<(), CodexError>>,
}

enum Command {
    Call(Execute),
    Respond(Respond),
}

impl Command {
    fn reject(self, message: &str) {
        match self {
            Self::Call(execute) => {
                let _ = execute.reply.send(Err(rejected(message)));
            }
            Self::Respond(respond) => {
                let _ = respond.reply.send(Err(rejected(message)));
            }
        }
    }
}

enum Write {
    Message(Message),
}

const TEXT_LIMIT: usize = 16_384;
const VALUE_LIMIT: usize = 16_384;
const ITEM_LIMIT: usize = 200;
const RING_LIMIT: usize = 20;

/// Large payloads (audio, long output) are kept as a marker so the state stays small.
fn bounded(value: &Value) -> Value {
    if value.to_string().len() <= VALUE_LIMIT {
        value.clone()
    } else {
        json!({"truncated":true})
    }
}

fn append(target: &mut Value, delta: &str) {
    let mut text = target.as_str().unwrap_or_default().to_owned();
    if text.len() < TEXT_LIMIT {
        text.push_str(delta);
    }
    *target = json!(text);
}

fn push_ring(ring: &mut VecDeque<Value>, value: Value) {
    if ring.len() == RING_LIMIT {
        ring.pop_front();
    }
    ring.push_back(value);
}

/// What the client has observed about one thread. `hidden` holds diffs and command output.
#[derive(Serialize)]
struct ThreadState {
    status: Value,
    name: Option<String>,
    cwd: Option<String>,
    archived: bool,
    settings: Value,
    goal: Value,
    active_turn: Option<String>,
    last_turn: Option<Value>,
    last_assistant: Option<String>,
    last_error: Value,
    plan: Value,
    reasoning: Vec<String>,
    token_usage: Value,
    items: BTreeMap<String, Value>,
    events: BTreeMap<String, Value>,
    hidden: BTreeMap<String, Value>,
    updated_at_ms: u64,
}

impl Default for ThreadState {
    fn default() -> Self {
        Self {
            status: json!({"type":"unknown"}),
            name: None,
            cwd: None,
            archived: false,
            settings: Value::Null,
            goal: Value::Null,
            active_turn: None,
            last_turn: None,
            last_assistant: None,
            last_error: Value::Null,
            plan: Value::Null,
            reasoning: Vec::new(),
            token_usage: Value::Null,
            items: BTreeMap::new(),
            events: BTreeMap::new(),
            hidden: BTreeMap::new(),
            updated_at_ms: now_ms(),
        }
    }
}

impl ThreadState {
    fn begin_turn(&mut self, turn: &Value) {
        self.active_turn = turn["id"].as_str().map(str::to_owned);
        self.last_assistant = None;
        self.last_error = Value::Null;
        self.plan = Value::Null;
        self.reasoning.clear();
        self.items.clear();
        self.hidden.clear();
    }

    fn item(&mut self, id: &str) -> &mut Value {
        if !self.items.contains_key(id) && self.items.len() >= ITEM_LIMIT {
            return self.hidden.entry("dropped_item".into()).or_default();
        }
        self.items
            .entry(id.into())
            .or_insert_with(|| json!({"id":id}))
    }
}

/// The observed state of one App Server: every notification lands here.
struct Cache {
    connection: Value,
    threads: BTreeMap<String, ThreadState>,
    requests: HashMap<RpcId, Value>,
    account: Value,
    rate_limits: Value,
    mcp: BTreeMap<String, Value>,
    apps: Value,
    skills_changed_at_ms: Option<u64>,
    warnings: VecDeque<Value>,
    events: BTreeMap<String, Value>,
    hidden: BTreeMap<String, Value>,
    /// Latest result of each thread-independent read (model list, config, account, skills...).
    reads: BTreeMap<String, Value>,
    replies: VecDeque<Value>,
}

impl Default for Cache {
    fn default() -> Self {
        Self {
            connection: json!({"state":"disconnected"}),
            threads: BTreeMap::new(),
            requests: HashMap::new(),
            account: Value::Null,
            rate_limits: Value::Null,
            mcp: BTreeMap::new(),
            apps: Value::Null,
            skills_changed_at_ms: None,
            warnings: VecDeque::new(),
            events: BTreeMap::new(),
            hidden: BTreeMap::new(),
            reads: BTreeMap::new(),
            replies: VecDeque::new(),
        }
    }
}

impl Cache {
    fn snapshot(&self) -> Value {
        let mut requests: Vec<_> = self.requests.values().collect();
        requests.sort_by_key(|request| request["id"].to_string());
        json!({"connection":self.connection,"threads":self.threads,"pending_requests":requests,
            "account":self.account,"rate_limits":self.rate_limits,"mcp":self.mcp,"apps":self.apps,
            "skills_changed_at_ms":self.skills_changed_at_ms,"warnings":self.warnings,
            "events":self.events,"hidden":self.hidden,"reads":self.reads,"replies":self.replies})
    }

    fn disconnect(&mut self, reason: &str) {
        self.connection = json!({"state":"disconnected","error":reason,"at_ms":now_ms()});
        self.requests.clear();
        for state in self.threads.values_mut() {
            state.status = json!({"type":"unknown"});
        }
    }

    fn observe_thread(&mut self, thread: &Value) {
        let Some(id) = thread["id"].as_str() else {
            return;
        };
        let state = self.threads.entry(id.into()).or_default();
        state.status = thread
            .get("status")
            .cloned()
            .unwrap_or(json!({"type":"unknown"}));
        if let Some(name) = thread["name"].as_str() {
            state.name = Some(name.into());
        }
        if let Some(cwd) = thread["cwd"].as_str() {
            state.cwd = Some(cwd.into());
        }
        state.updated_at_ms = now_ms();
        if let Some(turns) = thread["turns"].as_array() {
            for turn in turns {
                observe_turn(state, turn);
            }
        }
    }

    /// A read without a thread is global state worth keeping: models, config, account, skills.
    fn observe_read(&mut self, method: &str, params: &Value, result: &Value) {
        if methods::access(method) == methods::Access::Read && params.get("threadId").is_none() {
            self.reads.insert(method.into(), bounded(result));
        }
        if method == "account/read" {
            self.account = json!({"type":result["account"]["type"],"planType":result["account"]["planType"],
                "requiresOpenaiAuth":result["requiresOpenaiAuth"]});
            self.reads.remove(method);
        }
    }

    fn replied(&mut self, method: &str, thread: Option<&str>, decision: &str) {
        let reply = json!({"method":method,"threadId":thread,"decision":decision,"at_ms":now_ms()});
        push_ring(&mut self.replies, reply);
    }

    fn observe_event(&mut self, event: &Value) {
        let Some(method) = event["method"].as_str() else {
            return;
        };
        let request = request_id(event);
        if let Some(id) = &request {
            self.requests.insert(id.clone(), event.clone());
        }
        let params = &event["params"];
        let thread = params["threadId"].as_str();
        match (methods::category(method), thread) {
            (Some(Category::Request), _) => {
                if let Ok(id) = serde_json::from_value::<RpcId>(params["requestId"].clone()) {
                    self.requests.remove(&id);
                }
            }
            (Some(Category::Account), _) if method == "account/updated" => {
                self.account["authMode"] = params["authMode"].clone();
                self.account["planType"] = params["planType"].clone();
            }
            (Some(Category::RateLimits), _) => self.rate_limits = bounded(&params["rateLimits"]),
            (Some(Category::Apps), _) => self.apps = bounded(&params["data"]),
            (Some(Category::Skills), _) => self.skills_changed_at_ms = Some(now_ms()),
            (Some(Category::Mcp), _) if method == "mcpServer/startupStatus/updated" => {
                if let Some(name) = params["name"].as_str() {
                    self.mcp.insert(name.into(), bounded(params));
                }
            }
            (Some(Category::Warning), _) => {
                push_ring(
                    &mut self.warnings,
                    json!({"method":method,"params":bounded(params),"at_ms":now_ms()}),
                );
            }
            (Some(Category::Output), None) => {
                self.hidden.insert(method.into(), bounded(params));
            }
            (_, None) if method == "thread/started" => {}
            (_, None) => {
                self.events.insert(method.into(), bounded(params));
            }
            _ => {}
        }
        if method == "thread/started" {
            self.observe_thread(&params["thread"]);
        }
        let Some(id) = thread else {
            return;
        };
        let state = self.threads.entry(id.into()).or_default();
        state.updated_at_ms = now_ms();
        let item_id = params["itemId"].as_str();
        match method {
            "thread/status/changed" => state.status = params["status"].clone(),
            "thread/name/updated" => state.name = params["threadName"].as_str().map(str::to_owned),
            "thread/archived" => state.archived = true,
            "thread/unarchived" => state.archived = false,
            "thread/closed" | "thread/deleted" => state.status = json!({"type":"notLoaded"}),
            "thread/settings/updated" => state.settings = bounded(&params["threadSettings"]),
            "thread/goal/updated" => state.goal = bounded(&params["goal"]),
            "thread/goal/cleared" => state.goal = Value::Null,
            "thread/tokenUsage/updated" => state.token_usage = params["tokenUsage"].clone(),
            "turn/started" => {
                state.status = json!({"type":"active","activeFlags":[]});
                state.begin_turn(&params["turn"]);
                observe_turn(state, &params["turn"]);
            }
            "turn/completed" => {
                observe_turn(state, &params["turn"]);
                state.active_turn = None;
                let ended = turn_snapshot(&params["turn"])
                    .status
                    .is_some_and(|status| status.ended());
                state.status = json!({"type": if ended { "idle" } else { "unknown" }});
            }
            "error" => state.last_error = bounded(params),
            "turn/plan/updated" => {
                state.plan = json!({"turnId":params["turnId"],"explanation":params["explanation"],"steps":bounded(&params["plan"])});
            }
            "item/started" | "item/completed" => {
                if let Some(id) = params["item"]["id"].as_str() {
                    *state.item(id) = bounded(&params["item"]);
                }
                if method == "item/completed" {
                    observe_assistant(state, &params["item"]);
                }
            }
            "item/agentMessage/delta" | "item/plan/delta" => {
                if let (Some(id), Some(delta)) = (item_id, params["delta"].as_str()) {
                    append(&mut state.item(id)["text"], delta);
                }
            }
            "item/reasoning/summaryPartAdded" | "item/reasoning/summaryTextDelta" => {
                let index = params["summaryIndex"].as_u64().unwrap_or(0) as usize;
                if index < 64 {
                    if state.reasoning.len() <= index {
                        state.reasoning.resize(index + 1, String::new());
                    }
                    if let Some(delta) = params["delta"].as_str()
                        && state.reasoning[index].len() < TEXT_LIMIT
                    {
                        state.reasoning[index].push_str(delta);
                    }
                }
            }
            "turn/diff/updated" => {
                state.hidden.insert("diff".into(), bounded(&params["diff"]));
            }
            method if methods::category(method) == Some(Category::Output) => {
                if let (Some(id), Some(delta)) = (item_id, params["delta"].as_str()) {
                    append(
                        state.hidden.entry(format!("output:{id}")).or_default(),
                        delta,
                    );
                } else {
                    state.hidden.insert(method.into(), bounded(params));
                }
            }
            method
                if request.is_some()
                    && (method.ends_with("requestApproval")
                        || method == "item/tool/requestUserInput") =>
            {
                if let Some(turn) = params["turnId"].as_str() {
                    let terminal = state.last_turn.as_ref().is_some_and(|known| {
                        known["id"] == turn
                            && turn_snapshot(known)
                                .status
                                .is_some_and(|status| status.ended())
                    });
                    if !terminal {
                        if state
                            .last_turn
                            .as_ref()
                            .is_none_or(|known| known["id"] != turn)
                        {
                            state.last_assistant = None;
                        }
                        state.status = json!({"type":"active","activeFlags":[]});
                        state.last_turn = Some(
                            json!({"id":turn,"status":TurnStatus::InProgress.as_str(),"error":null}),
                        );
                    }
                }
            }
            "thread/started" | "serverRequest/resolved" => {}
            method => {
                state.events.insert(method.into(), bounded(params));
            }
        }
    }
}

fn observe_turn(state: &mut ThreadState, turn: &Value) {
    if turn["id"].is_string() {
        state.last_turn =
            Some(json!({"id":turn["id"],"status":turn["status"],"error":turn["error"]}));
    }
    if let Some(items) = turn["items"].as_array() {
        for item in items {
            observe_assistant(state, item);
        }
    }
}

fn observe_assistant(state: &mut ThreadState, item: &Value) {
    if item["type"] == "agentMessage"
        && (item["phase"].is_null() || item["phase"] == "final_answer")
        && let Some(text) = item["text"].as_str()
    {
        state.last_assistant = Some(text.chars().take(4_000).collect());
    }
}

fn response(id: &RpcId, result: &Result<Value, Value>) -> Value {
    match result {
        Ok(result) => json!({"id":id,"result":result}),
        Err(error) => json!({"id":id,"error":error}),
    }
}

fn request_id(message: &Value) -> Option<RpcId> {
    message.get("method")?.as_str()?;
    serde_json::from_value(message.get("id")?.clone()).ok()
}

enum PendingKind {
    Initialize,
    Request(Execute),
    ObserveThread(String),
    ObserveTurns(String),
    SubscribeThread(String),
}

struct Pending {
    kind: PendingKind,
    deadline: Instant,
    uncertain: bool,
}

#[derive(Default)]
struct Session {
    next_id: u64,
    ready: bool,
    pending: HashMap<RpcId, Pending>,
    subscribed: HashSet<String>,
    resuming: HashSet<String>,
    /// Threads another process writes: they cannot be resumed here, so their turns are read.
    polled: HashSet<String>,
    retry_observation_at: HashMap<String, Instant>,
    /// Every observed thread was read once on this connection, so any request the App Server
    /// asks again on them has arrived.
    observed_once: bool,
}

impl Session {
    fn request(
        &mut self,
        writer: &mpsc::UnboundedSender<Write>,
        method: &str,
        params: Value,
        kind: PendingKind,
        uncertain: bool,
    ) -> Result<(), String> {
        self.next_id += 1;
        let id = RpcId::String(format!("vibeface-{}", self.next_id));
        let message = json!({"id":id,"method":method,"params":params});
        self.pending.insert(
            id,
            Pending {
                kind,
                uncertain,
                deadline: Instant::now() + RPC_TIMEOUT,
            },
        );
        writer
            .send(Write::Message(Message::Text(message.to_string().into())))
            .map_err(|_| "Codex writer is closed".into())
    }

    fn command(
        &mut self,
        writer: &mpsc::UnboundedSender<Write>,
        execute: Execute,
    ) -> Result<(), String> {
        let method = execute.call.method.clone();
        let params = execute.call.params.clone();
        let uncertain = execute.call.uncertain();
        self.request(
            writer,
            &method,
            params,
            PendingKind::Request(execute),
            uncertain,
        )
    }

    fn observe_threads(
        &mut self,
        writer: &mpsc::UnboundedSender<Write>,
        threads: &BTreeSet<String>,
    ) -> Result<(), String> {
        if !self.ready {
            return Ok(());
        }
        self.retry_observation_at
            .retain(|id, _| threads.contains(id));
        for id in threads {
            if self.resuming.contains(id)
                || self
                    .retry_observation_at
                    .get(id)
                    .is_some_and(|at| *at > Instant::now())
            {
                continue;
            }
            self.resuming.insert(id.clone());
            self.request(
                writer,
                "thread/read",
                json!({"threadId":id,"includeTurns":false}),
                PendingKind::ObserveThread(id.clone()),
                false,
            )?;
        }
        Ok(())
    }

    fn fail(&mut self, pending: Pending, reason: &str, uncertain: bool) {
        match pending.kind {
            PendingKind::Request(execute) => {
                let _ = execute.reply.send(Err(CodexError {
                    uncertain: uncertain && pending.uncertain,
                    answered: !uncertain,
                    message: reason.into(),
                }));
            }
            PendingKind::SubscribeThread(thread) if writer_busy(reason) => {
                self.resuming.remove(&thread);
                self.polled.insert(thread);
            }
            PendingKind::SubscribeThread(thread)
            | PendingKind::ObserveThread(thread)
            | PendingKind::ObserveTurns(thread) => {
                self.resuming.remove(&thread);
                self.retry_observation_at
                    .insert(thread, Instant::now() + Duration::from_secs(5));
            }
            PendingKind::Initialize => {}
        }
    }

    fn disconnect(&mut self, reason: &str) {
        for (_, pending) in std::mem::take(&mut self.pending) {
            self.fail(pending, reason, true);
        }
        self.subscribed.clear();
        self.ready = false;
    }
}

/// Waits for the next endpoint change; a dropped sender never changes it again.
async fn endpoint_changed(endpoint: &mut watch::Receiver<Option<PathBuf>>) {
    if endpoint.changed().await.is_err() {
        std::future::pending::<()>().await;
    }
}

async fn connection_loop(
    mut endpoint: watch::Receiver<Option<PathBuf>>,
    mut commands: mpsc::UnboundedReceiver<Command>,
    observed: watch::Receiver<BTreeSet<String>>,
    cache: Arc<RwLock<Cache>>,
    events: mpsc::UnboundedSender<ExecEvent>,
) {
    let mut backoff = 1;
    loop {
        let socket = endpoint.borrow_and_update().clone();
        let connect = async {
            match &socket {
                Some(socket) => connect(socket).await,
                None => Err("no Codex App Server is selected".to_string()),
            }
        };
        tokio::pin!(connect);
        let result = loop {
            tokio::select! {
                result = &mut connect => break result,
                command = commands.recv() => match command {
                    Some(command) => command.reject("Codex is disconnected"),
                    None => return,
                }
            }
        };
        let reason = match result {
            Ok(stream) => {
                let session = run_session(stream, &mut commands, &observed, &cache, &events);
                tokio::pin!(session);
                let reason = loop {
                    tokio::select! {
                        reason = &mut session => break reason,
                        () = endpoint_changed(&mut endpoint) => {
                            if *endpoint.borrow() != socket {
                                break "the Codex App Server endpoint changed".to_string();
                            }
                        }
                    }
                };
                if cache.read().await.connection["state"] == "connected" {
                    backoff = 1;
                }
                reason
            }
            Err(reason) => reason,
        };
        cache.write().await.disconnect(&reason);
        let _ = events.send(ExecEvent::Disconnected);
        let delay = tokio::time::sleep(Duration::from_secs(backoff));
        tokio::pin!(delay);
        loop {
            tokio::select! {
                _ = &mut delay => break,
                () = endpoint_changed(&mut endpoint) => {
                    backoff = 0;
                    break;
                }
                command = commands.recv() => match command {
                    Some(command) => command.reject("Codex is disconnected"),
                    None => return,
                }
            }
        }
        backoff = (backoff * 2).clamp(1, 30);
    }
}

async fn run_session(
    stream: WebSocketStream<UnixStream>,
    commands: &mut mpsc::UnboundedReceiver<Command>,
    observed: &watch::Receiver<BTreeSet<String>>,
    cache: &Arc<RwLock<Cache>>,
    events: &mpsc::UnboundedSender<ExecEvent>,
) -> String {
    let (mut sink, mut reader) = stream.split();
    let (writer, mut writes) = mpsc::unbounded_channel();
    let writer_task = tokio::spawn(async move {
        while let Some(write) = writes.recv().await {
            let Write::Message(message) = write;
            timeout(RPC_TIMEOUT, sink.send(message))
                .await
                .map_err(|_| "Codex write timed out".to_string())
                .and_then(|result| result.map_err(|error| error.to_string()))?;
        }
        Ok::<(), String>(())
    });
    let mut writer_task = writer_task;
    let mut state = Session::default();
    let _ = state.request(
        &writer,
        "initialize",
        initialize_params("Vibeface Rust Bridge"),
        PendingKind::Initialize,
        false,
    );
    let mut ticker = tokio::time::interval(Duration::from_millis(250));
    let reason = loop {
        tokio::select! {
            writer_result = &mut writer_task => {
                break match writer_result {
                    Ok(Err(reason)) => reason,
                    Ok(Ok(())) => "Codex writer stopped".into(),
                    Err(error) => error.to_string(),
                };
            }
            command = commands.recv() => {
                let Some(command) = command else { break "Codex adapter stopped".into() };
                if !state.ready {
                    command.reject("Codex is initializing");
                    continue;
                }
                match command {
                    Command::Call(execute) => if let Err(reason) = state.command(&writer, execute) { break reason; },
                    Command::Respond(respond) => {
                        let mut observed_cache = cache.write().await;
                        let Some(request) = observed_cache.requests.remove(&respond.id) else {
                            let _ = respond.reply.send(Err(rejected("the request is no longer waiting for an answer")));
                            continue;
                        };
                        let message = response(&respond.id, &respond.result);
                        if writer.send(Write::Message(Message::Text(message.to_string().into()))).is_err() {
                            let _ = respond.reply.send(Err(CodexError { uncertain: true, answered: false, message: "Codex writer stopped".into() }));
                            break "Codex writer stopped".into();
                        }
                        observed_cache.replied(request["method"].as_str().unwrap_or_default(), request["params"]["threadId"].as_str(), &respond.decision);
                        let _ = respond.reply.send(Ok(()));
                    }
                }
            }
            message = reader.next() => {
                let text = match message {
                    Some(Ok(Message::Text(text))) => text,
                    Some(Ok(Message::Ping(data))) => {
                        if writer.send(Write::Message(Message::Pong(data))).is_err() { break "Codex writer stopped".into(); }
                        continue;
                    }
                    Some(Ok(Message::Close(_))) | None => break "Codex connection closed".into(),
                    Some(Err(error)) => break error.to_string(),
                    _ => continue,
                };
                let message: Value = match serde_json::from_str(&text) {
                    Ok(message) => message,
                    Err(error) => break format!("invalid Codex message: {error}"),
                };
                if let Some(method) = message["method"].as_str() {
                    let mut observed_cache = cache.write().await;
                    observed_cache.observe_event(&message);
                    let thread = message["params"]["threadId"].as_str();
                    // Requests on this Node's own threads, or with no thread at all, go to the Node
                    // to answer; the rest stay with the client that opened the thread.
                    if let Some(id) = request_id(&message)
                        && thread.is_none_or(|thread| observed.borrow().contains(thread))
                    {
                        let _ = events.send(ExecEvent::Request(ServerRequest { id, method: method.into(), thread_id: thread.map(str::to_owned), params: message["params"].clone() }));
                    }
                    drop(observed_cache);
                    if (message["method"] == "thread/closed" || message["method"] == "thread/unsubscribed")
                        && let Some(id) = message["params"]["threadId"].as_str() { state.subscribed.remove(id); }
                    let _ = events.send(ExecEvent::Message(message));
                    continue;
                }
                let Some(id) = message.get("id").and_then(|id| serde_json::from_value::<RpcId>(id.clone()).ok()) else { continue };
                let Some(pending) = state.pending.remove(&id) else { continue };
                if let Some(error) = message.get("error") {
                    let initialization = matches!(pending.kind, PendingKind::Initialize);
                    state.fail(pending, &server_error(error), false);
                    if initialization { break "Codex initialization rejected".into(); }
                    continue;
                }
                let Some(result) = message.get("result").cloned() else {
                    state.fail(pending, "Codex response has no result or error", true);
                    break "Codex response has no result or error".into();
                };
                match pending.kind {
                    PendingKind::Initialize => {
                        if writer.send(Write::Message(Message::Text(json!({"method":"initialized"}).to_string().into()))).is_err() { break "Codex writer stopped".into(); }
                        state.ready = true;
                        cache.write().await.connection = json!({"state":"connected","user_agent":result["userAgent"],"at_ms":now_ms()});
                        let _ = events.send(ExecEvent::Connected);
                    }
                    PendingKind::ObserveThread(thread_id) => {
                        cache.write().await.observe_thread(&result["thread"]);
                        if !state.subscribed.contains(&thread_id) && !state.polled.contains(&thread_id)
                            && matches!(result["thread"]["status"]["type"].as_str(), Some("idle" | "active" | "systemError")) {
                            if let Err(reason) = state.request(&writer, "thread/resume", json!({"threadId":thread_id,"excludeTurns":true}), PendingKind::SubscribeThread(thread_id), false) { break reason; }
                        } else if let Err(reason) = state.request(&writer, "thread/turns/list", json!({"threadId":thread_id,"limit":20,"itemsView":"full","sortDirection":"desc"}), PendingKind::ObserveTurns(thread_id), false) { break reason; }
                    }
                    PendingKind::SubscribeThread(thread_id) => {
                        cache.write().await.observe_thread(&result["thread"]);
                        state.subscribed.insert(thread_id.clone());
                        if let Err(reason) = state.request(&writer, "thread/turns/list", json!({"threadId":thread_id,"limit":20,"itemsView":"full","sortDirection":"desc"}), PendingKind::ObserveTurns(thread_id), false) { break reason; }
                    }
                    PendingKind::ObserveTurns(thread_id) => {
                        let mut cache = cache.write().await;
                        let thread = cache.threads.entry(thread_id.clone()).or_default();
                        if let Some(turns) = result["data"].as_array() {
                            for turn in turns.iter().rev() { observe_turn(thread, turn); }
                        }
                        state.resuming.remove(&thread_id);
                        state.retry_observation_at.insert(thread_id, Instant::now() + Duration::from_secs(5));
                    }
                    PendingKind::Request(execute) => {
                        let mut observed_cache = cache.write().await;
                        observed_cache.observe_thread(&result["thread"]);
                        observed_cache.observe_read(&execute.call.method, &execute.call.params, &result);
                        drop(observed_cache);
                        if matches!(execute.call.method.as_str(), "thread/start" | "thread/resume" | "thread/fork")
                            && let Some(id) = result["thread"]["id"].as_str() { state.subscribed.insert(id.into()); }
                        let _ = execute.reply.send(Ok(result));
                    }
                }
            }
            _ = ticker.tick() => {
                if let Err(reason) = state.observe_threads(&writer, &observed.borrow()) { break reason; }
                if state.ready && !state.observed_once && state.resuming.is_empty() {
                    state.observed_once = true;
                    let _ = events.send(ExecEvent::Observed);
                }
                let expired: Vec<_> = state.pending.iter().filter(|(_, pending)| pending.deadline <= Instant::now()).map(|(id, _)| id.clone()).collect();
                let mut initialize_expired = false;
                for id in expired {
                    if let Some(pending) = state.pending.remove(&id) {
                        initialize_expired |= matches!(pending.kind, PendingKind::Initialize);
                        state.fail(pending, "Codex request timed out; do not blindly retry mutations", true);
                    }
                }
                if initialize_expired { break "Codex initialization timed out".into(); }
            }
        }
    };
    writer_task.abort();
    state.disconnect(&reason);
    reason
}

async fn connect(socket: &Path) -> Result<WebSocketStream<UnixStream>, String> {
    let connect = async {
        let stream = UnixStream::connect(socket)
            .await
            .map_err(|error| error.to_string())?;
        client_async("ws://localhost/", stream)
            .await
            .map(|(stream, _)| stream)
            .map_err(|error| error.to_string())
    };
    timeout(Duration::from_secs(10), connect)
        .await
        .map_err(|_| "Codex connection timed out".to_string())?
}

fn initialize_params(title: &str) -> Value {
    json!({"clientInfo":{"name":"vibeface","title":title,"version":env!("CARGO_PKG_VERSION")},"capabilities":{"experimentalApi":true}})
}

/// A short-lived connection for one caller, such as the Agent's CLI. It leaves server requests to
/// the Node, which holds a lasting connection to every thread it watches.
pub struct DirectSession {
    stream: WebSocketStream<UnixStream>,
    next_id: u64,
    /// Threads started on this connection. They have no rollout before their first turn, so they
    /// are not resumed.
    started: HashSet<String>,
    /// Notifications that arrived while a request waited for its answer, oldest first.
    arrived: VecDeque<Value>,
    /// The `userAgent` the App Server answered `initialize` with.
    pub user_agent: String,
}

impl DirectSession {
    pub async fn connect(socket: &Path) -> Result<Self, CodexError> {
        let stream = connect(socket).await.map_err(rejected)?;
        let mut session = Self {
            stream,
            next_id: 0,
            started: HashSet::new(),
            arrived: VecDeque::new(),
            user_agent: String::new(),
        };
        let initialized = session
            .call("initialize", initialize_params("Vibeface Agent CLI"))
            .await
            .map_err(|error| rejected(error.message))?;
        session.user_agent = initialized["userAgent"].as_str().unwrap_or_default().into();
        session
            .stream
            .send(Message::Text(
                json!({"method":"initialized"}).to_string().into(),
            ))
            .await
            .map_err(|error| rejected(error.to_string()))?;
        Ok(session)
    }

    pub async fn request(&mut self, method: &str, params: Value) -> Result<Value, CodexError> {
        validate_request(method, &params)?;
        let result = self.call(method, params).await?;
        if method == "thread/start"
            && let Some(id) = result["thread"]["id"].as_str()
        {
            self.started.insert(id.to_owned());
        }
        Ok(result)
    }

    /// `thread/resume` to receive a thread's events. A thread without a rollout yet is retried
    /// until its first turn persists one.
    pub async fn subscribe(&mut self, thread: &str) -> Result<Value, CodexError> {
        loop {
            match self
                .request("thread/resume", json!({"threadId":thread}))
                .await
            {
                Err(error) if failure::no_rollout(&error.message) => {
                    tokio::time::sleep(Duration::from_secs(1)).await
                }
                result => return result,
            }
        }
    }

    async fn call(&mut self, method: &str, params: Value) -> Result<Value, CodexError> {
        let uncertain = methods::access(method) != methods::Access::Read;
        let lost = |message: String| CodexError {
            uncertain,
            answered: false,
            message,
        };
        self.next_id += 1;
        let id = RpcId::String(format!("vibeface-cli-{}", self.next_id));
        self.stream
            .send(Message::Text(
                json!({"id":id,"method":method,"params":params})
                    .to_string()
                    .into(),
            ))
            .await
            .map_err(|error| lost(error.to_string()))?;
        let deadline = Instant::now() + RPC_TIMEOUT;
        loop {
            let text = match tokio::time::timeout_at(deadline, self.stream.next()).await {
                Err(_) => {
                    return Err(lost(
                        "Codex request timed out; do not blindly retry mutations".into(),
                    ));
                }
                Ok(Some(Ok(Message::Text(text)))) => text,
                Ok(Some(Ok(Message::Ping(data)))) => {
                    let _ = self.stream.send(Message::Pong(data)).await;
                    continue;
                }
                Ok(Some(Ok(Message::Close(_))) | None) => {
                    return Err(lost("Codex connection closed".into()));
                }
                Ok(Some(Err(error))) => return Err(lost(error.to_string())),
                Ok(Some(Ok(_))) => continue,
            };
            let message: Value =
                serde_json::from_str(&text).map_err(|error| lost(error.to_string()))?;
            if request_id(&message).is_some() {
                continue;
            }
            if message["method"].is_string() {
                push_ring(&mut self.arrived, message);
                continue;
            }
            if message
                .get("id")
                .and_then(|got| serde_json::from_value::<RpcId>(got.clone()).ok())
                != Some(id.clone())
            {
                continue;
            }
            if let Some(error) = message.get("error") {
                return Err(answered(server_error(error)));
            }
            return message
                .get("result")
                .cloned()
                .ok_or_else(|| lost("Codex response has no result or error".into()));
        }
    }

    /// The next notification or server request on this connection.
    pub async fn notification(&mut self) -> Result<Value, CodexError> {
        if let Some(message) = self.arrived.pop_front() {
            return Ok(message);
        }
        loop {
            let text = match self.stream.next().await {
                Some(Ok(Message::Text(text))) => text,
                Some(Ok(Message::Ping(data))) => {
                    let _ = self.stream.send(Message::Pong(data)).await;
                    continue;
                }
                Some(Ok(Message::Close(_))) | None => {
                    return Err(rejected("Codex connection closed"));
                }
                Some(Err(error)) => return Err(rejected(error.to_string())),
                Some(Ok(_)) => continue,
            };
            let message: Value =
                serde_json::from_str(&text).map_err(|error| rejected(error.to_string()))?;
            if message["method"].is_string() {
                return Ok(message);
            }
        }
    }

    pub async fn close(mut self) {
        let _ = self.stream.close(None).await;
    }
}

/// A message for an existing thread. `client_id` is the caller's idempotency key: it travels as
/// `clientUserMessageId` and is how the send is found again.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct Outgoing {
    pub thread_id: String,
    pub text: String,
    pub client_id: String,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub effort: Option<String>,
    /// Local image paths, or `data:`/`http(s):` URLs.
    #[serde(default)]
    pub images: Vec<String>,
    /// Never steer: always a new turn (dispatched work).
    #[serde(default)]
    pub start_only: bool,
    /// Run the new turn with full access and no approvals (dispatched work).
    #[serde(default)]
    pub full_access: bool,
}

/// What starting a work came to, as its sender reports it to the Node.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum StartReport {
    /// The work's thread exists; nothing is sent into it yet.
    Thread { thread_id: String },
    /// The turn that carries the work's message.
    Turn { turn_id: String },
    /// Nothing was sent.
    Rejected(ExecFailure),
    /// The send may or may not have happened; sending again with the same `client_id` only
    /// reads back, and the Node reads the thread to tell.
    Unknown { reason: String },
}

fn input(outgoing: &Outgoing) -> Value {
    let mut input = vec![json!({"type":"text","text":outgoing.text,"text_elements":[]})];
    for image in &outgoing.images {
        input.push(
            if ["data:", "http://", "https://"]
                .iter()
                .any(|scheme| image.starts_with(scheme))
            {
                json!({"type":"image","url":image})
            } else {
                json!({"type":"localImage","path":image})
            },
        );
    }
    json!(input)
}

/// The turn holding the user message sent with `client_id`, if any.
fn delivered(thread: &Value, client_id: &str) -> Option<String> {
    thread["turns"].as_array()?.iter().find_map(|turn| {
        turn["items"]
            .as_array()?
            .iter()
            .any(|item| item["type"] == "userMessage" && item["clientId"] == client_id)
            .then(|| turn["id"].as_str().map(str::to_owned))?
    })
}

impl DirectSession {
    /// The turn that carries `client_id`, if the thread has it.
    pub async fn read_back(
        &mut self,
        thread: &str,
        client_id: &str,
    ) -> Result<Option<String>, CodexError> {
        let reply = self
            .request(
                "thread/read",
                json!({"threadId":thread,"includeTurns":true}),
            )
            .await?;
        Ok(delivered(&reply["thread"], client_id))
    }

    /// Resume, then steer the active turn or start one, then read back by `client_id`. A message
    /// already in the thread is reported, not sent again; an unknown result is never resent.
    /// Never `StartReport::Thread`: the thread exists already.
    pub async fn send(&mut self, outgoing: &Outgoing) -> StartReport {
        let rejected = |error: CodexError| StartReport::Rejected(error.failure());
        // A thread without a rollout has no turns: nothing to find or steer.
        let resumed = if self.started.contains(&outgoing.thread_id) {
            json!({})
        } else {
            match self
                .request("thread/resume", json!({"threadId":outgoing.thread_id}))
                .await
            {
                Ok(resumed) => resumed,
                // Codex writes a thread's rollout when its first turn starts.
                Err(error) if failure::no_rollout(&error.message) => json!({}),
                Err(error) => return rejected(error),
            }
        };
        if let Some(turn_id) = delivered(&resumed["thread"], &outgoing.client_id) {
            return StartReport::Turn { turn_id };
        }
        let active = (!outgoing.start_only && resumed["thread"]["status"]["type"] == "active")
            .then(|| {
                resumed["thread"]["turns"].as_array().and_then(|turns| {
                    turns
                        .iter()
                        .rev()
                        .find(|turn| turn_snapshot(turn).status == Some(TurnStatus::InProgress))
                        .and_then(|turn| turn["id"].as_str().map(str::to_owned))
                })
            })
            .flatten();
        let mut steered = None;
        if let Some(turn) = &active {
            match self
                .request(
                    "turn/steer",
                    json!({"threadId":outgoing.thread_id,"expectedTurnId":turn,
                    "input":input(outgoing),"clientUserMessageId":outgoing.client_id}),
                )
                .await
            {
                // Refused (the turn just ended): nothing was sent, so a new turn is safe.
                Err(error) if !error.uncertain => {}
                result => steered = Some(result),
            }
        }
        let result = match steered {
            Some(result) => result,
            None => {
                let mut params = json!({"threadId":outgoing.thread_id,"input":input(outgoing),
                    "clientUserMessageId":outgoing.client_id});
                if let Some(model) = &outgoing.model {
                    params["model"] = json!(model);
                }
                if let Some(effort) = &outgoing.effort {
                    params["effort"] = json!(effort);
                }
                if outgoing.full_access {
                    params["approvalPolicy"] = json!("never");
                    params["sandboxPolicy"] = json!({"type":"dangerFullAccess"});
                }
                self.request("turn/start", params).await
            }
        };
        let sent = match result {
            Ok(reply) => reply["turn"]["id"]
                .as_str()
                .or(reply["turnId"].as_str())
                .map(str::to_owned),
            Err(error) if !error.uncertain => return rejected(error),
            Err(_) => None,
        };
        match self
            .read_back(&outgoing.thread_id, &outgoing.client_id)
            .await
        {
            Ok(Some(turn_id)) => StartReport::Turn { turn_id },
            Ok(None) => match sent {
                Some(turn_id) => StartReport::Turn { turn_id },
                None => StartReport::Unknown {
                    reason: "the message is not in the thread yet".into(),
                },
            },
            Err(error) => match sent {
                Some(turn_id) => StartReport::Turn { turn_id },
                None => StartReport::Unknown {
                    reason: error.message,
                },
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn notifications_and_foreign_requests_flow_while_an_rpc_waits() {
        timeout(Duration::from_secs(3), async {
            let (left, right) = UnixStream::pair().unwrap();
            let (client, server) = tokio::join!(
                client_async("ws://localhost/", left),
                tokio_tungstenite::accept_async(right)
            );
            let websocket = client.unwrap().0;
            let (commands, mut receiver) = mpsc::unbounded_channel();
            let (events, mut received_events) = mpsc::unbounded_channel();
            let cache = Arc::new(RwLock::new(Cache::default()));
            let (observed_threads, observed) = watch::channel(BTreeSet::new());
            let client = CodexClient { commands, cache: cache.clone(), observed_threads };
            let session = tokio::spawn(async move {
                run_session(websocket, &mut receiver, &observed, &cache, &events).await
            });
            let mut server = server.unwrap();
            let initialize: Value = serde_json::from_str(server.next().await.unwrap().unwrap().to_text().unwrap()).unwrap();
            assert_eq!(initialize["method"], "initialize");
            assert_eq!(initialize["params"]["capabilities"]["experimentalApi"], true);
            assert!(initialize.get("jsonrpc").is_none());
            server.send(Message::Text(json!({"id":initialize["id"],"result":{"userAgent":"codex/0.163.0"}}).to_string().into())).await.unwrap();
            let initialized: Value = serde_json::from_str(server.next().await.unwrap().unwrap().to_text().unwrap()).unwrap();
            assert_eq!(initialized, json!({"method":"initialized"}));
            assert_eq!(received_events.recv().await.unwrap(), ExecEvent::Connected);
            assert_eq!(received_events.recv().await.unwrap(), ExecEvent::Observed, "with no thread to observe, observation ends at once");

            let querying = client.clone();
            let response = tokio::spawn(async move {
                querying.request("thread/read", json!({"threadId":"t","includeTurns":false})).await
            });
            let request: Value = serde_json::from_str(server.next().await.unwrap().unwrap().to_text().unwrap()).unwrap();
            assert_eq!(request["method"], "thread/read");
            let approval = json!({"id":27,"method":"item/commandExecution/requestApproval","params":{"threadId":"t"}});
            server.send(Message::Text(approval.to_string().into())).await.unwrap();
            assert_eq!(received_events.recv().await.unwrap(), ExecEvent::Message(approval));
            assert_eq!(client.snapshot().await["pending_requests"][0]["id"], 27);
            server.send(Message::Text(json!({"id":request["id"],"result":{"thread":{"id":"t","status":{"type":"idle"}}}}).to_string().into())).await.unwrap();
            assert_eq!(response.await.unwrap().unwrap()["thread"]["id"], "t");

            let refresh = json!({"id":28,"method":"account/chatgptAuthTokens/refresh","params":{"reason":"unauthorized"}});
            server.send(Message::Text(refresh.to_string().into())).await.unwrap();
            let ExecEvent::Request(handed) = received_events.recv().await.unwrap() else { panic!("a request without a thread goes to the Node") };
            assert_eq!(handed.id, RpcId::Number(28));
            assert_eq!(received_events.recv().await.unwrap(), ExecEvent::Message(refresh));
            client.respond(&json!(28), Err(json!({"code":-32601,"message":"no credentials"})), "refuse").await.unwrap();
            let sent: Value = serde_json::from_str(server.next().await.unwrap().unwrap().to_text().unwrap()).unwrap();
            assert_eq!(sent["id"], 28);
            assert!(sent["error"]["message"].is_string());
            assert!(client.respond(&json!(28), Ok(json!({})), "again").await.is_err(), "a request is answered once");
            assert_eq!(client.snapshot().await["pending_requests"].as_array().unwrap().len(), 1, "another client's approval stays for that client");
            server.close(None).await.unwrap();
            assert_eq!(session.await.unwrap(), "Codex connection closed");
        }).await.expect("mock Codex session must finish");
    }

    #[tokio::test]
    async fn a_thread_another_process_writes_is_read_and_a_new_endpoint_is_followed() {
        timeout(Duration::from_secs(8), async {
            let path = |name: &str| std::env::temp_dir().join(format!("vibeface-{name}-{}-{}.sock", std::process::id(), now_ms()));
            let (first, second) = (path("endpoint-a"), path("endpoint-b"));
            let (listener_a, listener_b) = (tokio::net::UnixListener::bind(&first).unwrap(), tokio::net::UnixListener::bind(&second).unwrap());
            let (endpoint, receiver) = watch::channel(Some(first.clone()));
            let (events, _events) = mpsc::unbounded_channel();
            let client = CodexClient::start(receiver, events);
            client.set_observed_threads(vec!["busy".into()]).unwrap();
            let mut server = tokio_tungstenite::accept_async(listener_a.accept().await.unwrap().0).await.unwrap();
            let initialize = next(&mut server).await;
            server.send(Message::Text(json!({"id":initialize["id"],"result":{"userAgent":"a"}}).to_string().into())).await.unwrap();
            assert_eq!(next(&mut server).await["method"], "initialized");
            let read = next(&mut server).await;
            server.send(Message::Text(json!({"id":read["id"],"result":{"thread":{"id":"busy","status":{"type":"idle"}}}}).to_string().into())).await.unwrap();
            let resume = next(&mut server).await;
            assert_eq!(resume["method"], "thread/resume");
            server.send(Message::Text(json!({"id":resume["id"],"error":{"code":-32600,"message":"thread busy already has an active writer"}}).to_string().into())).await.unwrap();
            let read = next(&mut server).await;
            server.send(Message::Text(json!({"id":read["id"],"result":{"thread":{"id":"busy","status":{"type":"idle"}}}}).to_string().into())).await.unwrap();
            assert_eq!(next(&mut server).await["method"], "thread/turns/list", "a thread another process writes is read, not resumed again");

            endpoint.send_replace(Some(second.clone()));
            let mut moved = tokio_tungstenite::accept_async(listener_b.accept().await.unwrap().0).await.unwrap();
            assert_eq!(next(&mut moved).await["method"], "initialize", "the client follows the new endpoint");
            assert!(!matches!(timeout(Duration::from_secs(2), server.next()).await.unwrap(), Some(Ok(Message::Text(_)))), "the old connection ends");
            std::fs::remove_file(first).unwrap();
            std::fs::remove_file(second).unwrap();
        }).await.expect("endpoint test must finish");
    }

    #[tokio::test]
    async fn known_threads_resubscribe_after_disconnect_without_replaying_a_turn() {
        timeout(Duration::from_secs(8), async {
            let socket = std::env::temp_dir().join(format!("vibeface-subscribe-{}-{}.sock", std::process::id(), now_ms()));
            let listener = tokio::net::UnixListener::bind(&socket).unwrap();
            let (events, mut received_events) = mpsc::unbounded_channel();
            let client = CodexClient::start(watch::channel(Some(socket.clone())).1, events);
            client.set_observed_threads(vec!["known".into()]).unwrap();
            let mut sent_turns = 0;
            for connection in 0..3 {
                let (stream, _) = listener.accept().await.unwrap();
                let mut server = tokio_tungstenite::accept_async(stream).await.unwrap();
                let initialize: Value = serde_json::from_str(server.next().await.unwrap().unwrap().to_text().unwrap()).unwrap();
                assert_eq!(initialize["method"], "initialize");
                server.send(Message::Text(json!({"id":initialize["id"],"result":{"userAgent":"test"}}).to_string().into())).await.unwrap();
                let initialized: Value = serde_json::from_str(server.next().await.unwrap().unwrap().to_text().unwrap()).unwrap();
                assert_eq!(initialized["method"], "initialized");
                let read: Value = serde_json::from_str(server.next().await.unwrap().unwrap().to_text().unwrap()).unwrap();
                assert_eq!(read["method"], "thread/read");
                let status = if connection == 2 { "notLoaded" } else { "active" };
                server.send(Message::Text(json!({"id":read["id"],"result":{"thread":{"id":"known","status":{"type":status}}}}).to_string().into())).await.unwrap();
                if connection != 2 {
                    let resume: Value = serde_json::from_str(server.next().await.unwrap().unwrap().to_text().unwrap()).unwrap();
                    assert_eq!(resume["method"], "thread/resume");
                    assert_eq!(resume["params"], json!({"threadId":"known","excludeTurns":true}));
                    server.send(Message::Text(json!({"id":resume["id"],"result":{"thread":{"id":"known","status":{"type":"active"}}}}).to_string().into())).await.unwrap();
                }
                let turns: Value = serde_json::from_str(server.next().await.unwrap().unwrap().to_text().unwrap()).unwrap();
                assert_eq!(turns["method"], "thread/turns/list");
                while let Ok(event) = received_events.try_recv() {
                    assert_ne!(event, ExecEvent::Observed, "observation ends only once every thread was read");
                }
                server.send(Message::Text(json!({"id":turns["id"],"result":{"data":[{"id":"previous-turn","status":"completed","items":[]}]}}).to_string().into())).await.unwrap();
                while received_events.recv().await.unwrap() != ExecEvent::Observed {}
                if connection == 0 {
                    let sender = client.clone();
                    let sending = tokio::spawn(async move {
                        sender.request("turn/start", json!({"threadId":"known","input":[{"type":"text","text":"write once","text_elements":[]}]})).await
                    });
                    let start: Value = serde_json::from_str(server.next().await.unwrap().unwrap().to_text().unwrap()).unwrap();
                    assert_eq!(start["method"], "turn/start");
                    sent_turns += 1;
                    server.close(None).await.unwrap();
                    assert!(sending.await.unwrap().unwrap_err().uncertain);
                } else if connection == 1 {
                    let approval = json!({"id":42,"method":"item/commandExecution/requestApproval","params":{"threadId":"known"}});
                    let completed = json!({"method":"turn/completed","params":{"threadId":"known","turn":{"id":"turn","status":"failed","error":{"message":"declined"},"items":[]}}});
                    server.send(Message::Text(approval.to_string().into())).await.unwrap();
                    server.send(Message::Text(completed.to_string().into())).await.unwrap();
                    let mut recovered = Vec::new();
                    while recovered.len() < 2 {
                        if let ExecEvent::Message(event) = received_events.recv().await.unwrap()
                            && (event == approval || event == completed)
                        {
                            recovered.push(event);
                        }
                    }
                    assert_eq!(recovered, vec![approval, completed]);
                    assert!(timeout(Duration::from_millis(200), server.next()).await.is_err(), "the bridge itself answers nothing");
                    client.respond(&json!(42), Ok(json!({"decision":"accept"})), "accept").await.unwrap();
                    let reply: Value = serde_json::from_str(server.next().await.unwrap().unwrap().to_text().unwrap()).unwrap();
                    assert_eq!(reply, json!({"id":42,"result":{"decision":"accept"}}), "the Node's answer reaches the App Server");
                    let snapshot = client.snapshot().await;
                    assert!(snapshot["pending_requests"].as_array().unwrap().is_empty());
                    assert_eq!(snapshot["replies"][0]["decision"], "accept");
                    client.set_observed_threads(vec!["known".into(), "known".into()]).unwrap();
                    assert!(timeout(Duration::from_millis(300), server.next()).await.is_err());
                    server.close(None).await.unwrap();
                } else {
                    assert!(timeout(Duration::from_millis(300), server.next()).await.is_err());
                    let snapshot = client.snapshot().await;
                    assert_eq!(snapshot["threads"]["known"]["status"]["type"], "notLoaded");
                    assert_eq!(snapshot["threads"]["known"]["last_turn"]["id"], "previous-turn");
                    server.close(None).await.unwrap();
                }
            }
            assert_eq!(sent_turns, 1);
            drop(client);
            std::fs::remove_file(socket).unwrap();
        }).await.expect("Codex reconnect must restore observation without replaying a mutation");
    }

    #[test]
    fn native_calls_validate_only_the_transport_boundary() {
        assert!(
            validate_request("future/nativeMethod", &json!({"unmodified":[1,true,null]})).is_ok()
        );
        assert!(validate_request("bad\nmethod", &json!({})).is_err());
        assert!(validate_request("thread/read", &json!("x".repeat(1_048_577))).is_err());
        assert!(
            !NativeCall {
                method: "thread/read".into(),
                params: json!({})
            }
            .uncertain()
        );
        assert!(
            NativeCall {
                method: "future/nativeMethod".into(),
                params: json!({})
            }
            .uncertain()
        );
    }

    #[test]
    fn reverse_requests_preserve_id_type_and_resolved_requests_disappear() {
        let mut cache = Cache::default();
        cache.observe_event(&json!({"id":7,"method":"item/commandExecution/requestApproval","params":{"threadId":"t"}}));
        cache.observe_event(
            &json!({"id":"7","method":"item/tool/requestUserInput","params":{"threadId":"t"}}),
        );
        assert_eq!(cache.requests.len(), 2);
        assert_eq!(cache.requests[&RpcId::Number(7)]["id"], 7);
        cache.observe_event(
            &json!({"method":"serverRequest/resolved","params":{"threadId":"t","requestId":7}}),
        );
        assert!(!cache.requests.contains_key(&RpcId::Number(7)));
        assert!(cache.requests.contains_key(&RpcId::String("7".into())));
        assert!(request_id(&json!({"id":7,"result":{}})).is_none());
    }

    #[tokio::test]
    async fn only_a_server_error_reply_is_an_answer() {
        let mut state = Session::default();
        let (writer, _reader) = mpsc::unbounded_channel();
        let read = |state: &mut Session| {
            let (reply, result) = oneshot::channel();
            state
                .command(
                    &writer,
                    Execute {
                        call: NativeCall {
                            method: "thread/read".into(),
                            params: json!({"threadId":"t"}),
                        },
                        reply,
                    },
                )
                .unwrap();
            result
        };
        let rejected = read(&mut state);
        let (_, pending) = state.pending.drain().next().unwrap();
        state.fail(pending, "thread not found", false);
        let error = rejected.await.unwrap().unwrap_err();
        assert!(error.answered && !error.uncertain);
        let lost = read(&mut state);
        state.disconnect("socket closed");
        let error = lost.await.unwrap().unwrap_err();
        assert!(
            !error.answered && !error.uncertain,
            "a lost read is neither an answer nor a write"
        );
    }

    #[tokio::test]
    async fn disconnect_fails_pending_mutation_as_uncertain_and_clears_live_state() {
        let mut state = Session::default();
        let (writer, _reader) = mpsc::unbounded_channel();
        let (reply, result) = oneshot::channel();
        state
            .command(
                &writer,
                Execute {
                    call: NativeCall {
                        method: "turn/interrupt".into(),
                        params: json!({"threadId":"t","turnId":"r"}),
                    },
                    reply,
                },
            )
            .unwrap();
        state.subscribed.insert("t".into());
        state.disconnect("socket closed");
        let lost = result.await.unwrap().unwrap_err();
        assert!(lost.uncertain && !lost.answered);
        assert!(state.pending.is_empty());
        assert!(state.subscribed.is_empty());
        let mut cache = Cache::default();
        cache.observe_event(&json!({"id":1,"method":"approval","params":{"threadId":"t"}}));
        cache.disconnect("socket closed");
        assert!(cache.requests.is_empty());
        assert_eq!(
            cache.snapshot()["threads"]["t"]["status"]["type"],
            "unknown"
        );
    }

    #[test]
    fn live_input_request_recovers_exact_turn_without_reversing_a_terminal() {
        for method in [
            "item/commandExecution/requestApproval",
            "item/tool/requestUserInput",
        ] {
            let mut cache = Cache::default();
            let request = json!({"id":0,"method":method,"params":{"threadId":"t","turnId":"r"}});
            cache.observe_event(&request);
            let snapshot = cache.snapshot();
            assert_eq!(snapshot["threads"]["t"]["status"]["type"], "active");
            assert_eq!(
                snapshot["threads"]["t"]["last_turn"],
                json!({"id":"r","status":"inProgress","error":null})
            );
            assert_eq!(snapshot["pending_requests"], json!([request.clone()]));
            cache.observe_event(&json!({"method":"turn/completed","params":{"threadId":"t","turn":{"id":"r","status":"interrupted","items":[]}}}));
            cache.observe_event(&request);
            let snapshot = cache.snapshot();
            assert_eq!(snapshot["threads"]["t"]["status"]["type"], "idle");
            assert_eq!(
                snapshot["threads"]["t"]["last_turn"]["status"],
                "interrupted"
            );
        }
        let mut cache = Cache::default();
        cache.observe_event(&json!({"method":"item/commandExecution/requestApproval","params":{"threadId":"t","turnId":"r"}}));
        assert!(cache.snapshot()["threads"]["t"]["last_turn"].is_null());
    }

    #[test]
    fn completed_turn_keeps_failed_status_and_final_text() {
        let mut cache = Cache::default();
        cache.observe_event(&json!({"method":"turn/completed","params":{"threadId":"t","turn":{"id":"r","status":"failed","error":{"message":"blocked"},"items":[{"type":"agentMessage","phase":"commentary","text":"working"},{"type":"agentMessage","phase":"final_answer","text":"Needs credentials"}]}}}));
        let snapshot = cache.snapshot();
        assert_eq!(snapshot["threads"]["t"]["last_turn"]["status"], "failed");
        assert_eq!(
            snapshot["threads"]["t"]["last_assistant"],
            "Needs credentials"
        );
        assert_eq!(snapshot["threads"]["t"]["status"]["type"], "idle");
    }

    #[test]
    fn notifications_fill_the_observed_state_and_sanitize_hides_execution() {
        let mut cache = Cache::default();
        for (method, params) in [
            (
                "turn/started",
                json!({"threadId":"t","turn":{"id":"r","status":"inProgress","items":[]}}),
            ),
            (
                "thread/name/updated",
                json!({"threadId":"t","threadName":"fix login"}),
            ),
            (
                "thread/tokenUsage/updated",
                json!({"threadId":"t","turnId":"r","tokenUsage":{"total":{"totalTokens":42}}}),
            ),
            (
                "turn/plan/updated",
                json!({"threadId":"t","turnId":"r","explanation":null,"plan":[{"step":"read","status":"completed"}]}),
            ),
            (
                "item/reasoning/summaryTextDelta",
                json!({"threadId":"t","turnId":"r","itemId":"z","summaryIndex":0,"delta":"thinking"}),
            ),
            (
                "item/started",
                json!({"threadId":"t","turnId":"r","item":{"id":"c","type":"commandExecution","status":"inProgress","command":"cat secret"}}),
            ),
            (
                "item/commandExecution/outputDelta",
                json!({"threadId":"t","turnId":"r","itemId":"c","delta":"output"}),
            ),
            (
                "turn/diff/updated",
                json!({"threadId":"t","turnId":"r","diff":"+x"}),
            ),
            (
                "account/rateLimits/updated",
                json!({"rateLimits":{"primary":{"usedPercent":10}}}),
            ),
            (
                "account/updated",
                json!({"authMode":"chatgpt","planType":"pro"}),
            ),
            (
                "mcpServer/startupStatus/updated",
                json!({"name":"docs","status":"ready","threadId":null}),
            ),
            ("warning", json!({"message":"slow","threadId":null})),
            ("thread/realtime/started", json!({"threadId":"t"})),
        ] {
            cache.observe_event(&json!({"method":method,"params":params}));
        }
        let mut snapshot = cache.snapshot();
        let thread = &snapshot["threads"]["t"];
        assert_eq!(thread["name"], "fix login");
        assert_eq!(thread["token_usage"]["total"]["totalTokens"], 42);
        assert_eq!(thread["plan"]["steps"][0]["step"], "read");
        assert_eq!(thread["reasoning"][0], "thinking");
        assert_eq!(thread["hidden"]["diff"], "+x");
        assert_eq!(thread["hidden"]["output:c"], "output");
        assert!(thread["events"]["thread/realtime/started"].is_object());
        assert_eq!(snapshot["rate_limits"]["primary"]["usedPercent"], 10);
        assert_eq!(snapshot["account"]["planType"], "pro");
        assert_eq!(snapshot["mcp"]["docs"]["status"], "ready");
        assert_eq!(snapshot["warnings"][0]["params"]["message"], "slow");
        methods::sanitize(&mut snapshot);
        let text = snapshot.to_string();
        assert!(!text.contains("cat secret") && !text.contains("output") && !text.contains("+x"));
        assert_eq!(
            snapshot["threads"]["t"]["items"]["c"],
            json!({"id":"c","type":"commandExecution","status":"inProgress"})
        );
    }

    async fn next(server: &mut WebSocketStream<UnixStream>) -> Value {
        serde_json::from_str(server.next().await.unwrap().unwrap().to_text().unwrap()).unwrap()
    }

    #[tokio::test]
    async fn direct_session_leaves_requests_and_returns_its_result() {
        timeout(Duration::from_secs(5), async {
            let socket = std::env::temp_dir().join(format!("vibeface-direct-{}-{}.sock", std::process::id(), now_ms()));
            let listener = tokio::net::UnixListener::bind(&socket).unwrap();
            let server = tokio::spawn(async move {
                let (stream, _) = listener.accept().await.unwrap();
                let mut server = tokio_tungstenite::accept_async(stream).await.unwrap();
                let initialize = next(&mut server).await;
                assert_eq!(initialize["params"]["clientInfo"]["title"], "Vibeface Agent CLI");
                server.send(Message::Text(json!({"id":initialize["id"],"result":{}}).to_string().into())).await.unwrap();
                assert_eq!(next(&mut server).await["method"], "initialized");
                let start = next(&mut server).await;
                assert_eq!(start["params"]["clientUserMessageId"], "work-1");
                server.send(Message::Text(json!({"id":5,"method":"item/fileChange/requestApproval","params":{"threadId":"t"}}).to_string().into())).await.unwrap();
                server.send(Message::Text(json!({"id":start["id"],"result":{"turn":{"id":"r"}}}).to_string().into())).await.unwrap();
            });
            let mut session = DirectSession::connect(&socket).await.unwrap();
            let reply = session.request("turn/start", json!({"threadId":"t","clientUserMessageId":"work-1","input":[]})).await.unwrap();
            assert_eq!(reply["turn"]["id"], "r");
            server.await.unwrap();
            let error = session.request("turn/start", json!({})).await.unwrap_err();
            assert!(error.uncertain, "a lost write reply is an unknown result");
            std::fs::remove_file(socket).unwrap();
        }).await.expect("direct session must finish");
    }

    #[tokio::test]
    async fn send_steers_an_active_turn_and_never_sends_twice() {
        timeout(Duration::from_secs(5), async {
            let socket = std::env::temp_dir().join(format!("vibeface-send-{}-{}.sock", std::process::id(), now_ms()));
            let listener = tokio::net::UnixListener::bind(&socket).unwrap();
            let server = tokio::spawn(async move {
                let mut sent = 0;
                for connection in 0..2 {
                    let (stream, _) = listener.accept().await.unwrap();
                    let mut server = tokio_tungstenite::accept_async(stream).await.unwrap();
                    let initialize = next(&mut server).await;
                    server.send(Message::Text(json!({"id":initialize["id"],"result":{}}).to_string().into())).await.unwrap();
                    next(&mut server).await;
                    let message = json!({"type":"userMessage","clientId":"d1","content":[]});
                    let items = if connection == 0 { json!([]) } else { json!([message]) };
                    let resume = next(&mut server).await;
                    assert_eq!(resume["method"], "thread/resume");
                    server.send(Message::Text(json!({"id":resume["id"],"result":{"thread":{"id":"t","status":{"type":"active"},
                        "turns":[{"id":"r1","status":"inProgress","items":items}]}}}).to_string().into())).await.unwrap();
                    if connection == 1 {
                        continue;
                    }
                    let steer = next(&mut server).await;
                    assert_eq!((steer["method"].as_str(), steer["params"]["expectedTurnId"].as_str(), steer["params"]["clientUserMessageId"].as_str()), (Some("turn/steer"), Some("r1"), Some("d1")));
                    sent += 1;
                    server.send(Message::Text(json!({"id":steer["id"],"result":{"turnId":"r1"}}).to_string().into())).await.unwrap();
                    let read = next(&mut server).await;
                    assert_eq!(read["method"], "thread/read");
                    server.send(Message::Text(json!({"id":read["id"],"result":{"thread":{"id":"t","turns":[{"id":"r1","items":[message]}]}}}).to_string().into())).await.unwrap();
                }
                sent
            });
            let outgoing = Outgoing { thread_id: "t".into(), text: "use plan B".into(), client_id: "d1".into(), ..Default::default() };
            for _ in 0..2 {
                let mut session = DirectSession::connect(&socket).await.unwrap();
                assert_eq!(session.send(&outgoing).await, StartReport::Turn { turn_id: "r1".into() });
                session.close().await;
            }
            assert_eq!(server.await.unwrap(), 1, "a repeated send only reads back");
            std::fs::remove_file(socket).unwrap();
        }).await.expect("send must finish");
    }

    #[tokio::test]
    async fn dispatch_to_a_new_thread_delivers_the_first_turn() {
        timeout(Duration::from_secs(5), async {
            let socket = std::env::temp_dir().join(format!("vibeface-fresh-{}-{}.sock", std::process::id(), now_ms()));
            let listener = tokio::net::UnixListener::bind(&socket).unwrap();
            let server = tokio::spawn(async move {
                let mut turns: HashMap<String, Vec<Value>> = HashMap::new();
                loop {
                    let (stream, _) = listener.accept().await.unwrap();
                    let mut server = tokio_tungstenite::accept_async(stream).await.unwrap();
                    while let Some(Ok(Message::Text(text))) = server.next().await {
                        let request: Value = serde_json::from_str(&text).unwrap();
                        let Some(method) = request["method"].as_str().map(str::to_owned) else { continue };
                        if request.get("id").is_none() { continue }
                        let thread = request["params"]["threadId"].as_str().unwrap_or_default().to_owned();
                        let reply = match method.as_str() {
                            "thread/start" => {
                                let id = format!("fresh-{}", turns.len());
                                turns.insert(id.clone(), Vec::new());
                                json!({"result":{"thread":{"id":id,"status":{"type":"idle"},"turns":[]}}})
                            }
                            "thread/resume" if turns[&thread].is_empty() =>
                                json!({"error":{"code":-32600,"message":format!("no rollout found for thread id {thread}")}}),
                            "turn/start" => {
                                let turn = json!({"id":format!("turn-{thread}"),"status":"inProgress",
                                    "items":[{"type":"userMessage","clientId":request["params"]["clientUserMessageId"]}]});
                                turns.get_mut(&thread).unwrap().push(turn.clone());
                                json!({"result":{"turn":turn}})
                            }
                            "thread/read" | "thread/resume" =>
                                json!({"result":{"thread":{"id":thread,"status":{"type":"idle"},"turns":turns[&thread]}}}),
                            _ => json!({"result":{}}),
                        };
                        let mut reply = reply;
                        reply["id"] = request["id"].clone();
                        server.send(Message::Text(reply.to_string().into())).await.unwrap();
                    }
                }
            });
            let mut session = DirectSession::connect(&socket).await.unwrap();
            let started = session.request("thread/start", json!({"cwd":"/project","approvalPolicy":"never","sandbox":"danger-full-access"})).await.unwrap();
            let thread = started["thread"]["id"].as_str().unwrap().to_owned();
            let outgoing = Outgoing { thread_id: thread, text: "do the work".into(), client_id: "w1".into(), start_only: true, full_access: true, ..Default::default() };
            assert_eq!(session.send(&outgoing).await, StartReport::Turn { turn_id: "turn-fresh-0".into() },
                "the thread this session started gets its first turn");
            session.close().await;

            let mut session = DirectSession::connect(&socket).await.unwrap();
            session.request("thread/start", json!({"cwd":"/project"})).await.unwrap();
            session.close().await;
            let mut session = DirectSession::connect(&socket).await.unwrap();
            let outgoing = Outgoing { thread_id: "fresh-1".into(), text: "hello".into(), client_id: "m1".into(), ..Default::default() };
            assert_eq!(session.send(&outgoing).await, StartReport::Turn { turn_id: "turn-fresh-1".into() },
                "a thread started elsewhere without a rollout yet still gets its first turn");
            session.close().await;
            server.abort();
            std::fs::remove_file(socket).unwrap();
        }).await.expect("send must finish");
    }
}
