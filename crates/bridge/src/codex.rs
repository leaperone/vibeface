use futures_util::{SinkExt, StreamExt};
use serde::Serialize;
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet, HashMap, HashSet},
    fmt,
    path::PathBuf,
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
    pub message: String,
}

impl fmt::Display for CodexError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for CodexError {}

fn rejected(message: impl Into<String>) -> CodexError {
    CodexError {
        uncertain: false,
        message: message.into(),
    }
}

#[derive(Clone)]
pub struct CodexClient {
    commands: mpsc::UnboundedSender<Execute>,
    cache: Arc<RwLock<Cache>>,
    observed_threads: watch::Sender<BTreeSet<String>>,
}

impl CodexClient {
    pub fn start(socket: PathBuf, events: mpsc::UnboundedSender<Value>) -> Self {
        let (commands, receiver) = mpsc::unbounded_channel();
        let cache = Arc::new(RwLock::new(Cache::default()));
        let (observed_threads, observed) = watch::channel(BTreeSet::new());
        tokio::spawn(connection_loop(
            socket,
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
        self.send(NativeCall::Request {
            method: method.into(),
            params,
        })
        .await
    }

    pub async fn respond(&self, id: Value, result: Value) -> Result<(), CodexError> {
        let id = serde_json::from_value(id).map_err(|_| rejected("invalid native request id"))?;
        if serde_json::to_vec(&result)
            .map_err(|error| rejected(error.to_string()))?
            .len()
            > 1_048_576
        {
            return Err(rejected("native response is too large"));
        }
        self.send(NativeCall::Respond { id, result })
            .await
            .map(|_| ())
    }

    async fn send(&self, call: NativeCall) -> Result<Value, CodexError> {
        let uncertain = call.uncertain();
        let (reply, response) = oneshot::channel();
        self.commands
            .send(Execute { call, reply })
            .map_err(|_| rejected("Codex adapter is stopped"))?;
        response.await.unwrap_or_else(|_| {
            Err(CodexError {
                uncertain,
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
}

enum NativeCall {
    Request { method: String, params: Value },
    Respond { id: RpcId, result: Value },
}

impl NativeCall {
    fn uncertain(&self) -> bool {
        match self {
            Self::Request { method, .. } => !matches!(
                method.as_str(),
                "thread/read"
                    | "thread/list"
                    | "thread/loaded/list"
                    | "thread/turns/list"
                    | "thread/items/list"
                    | "thread/items/read"
                    | "thread/goal/get"
                    | "thread/backgroundTerminals/list"
                    | "thread/attachment/list"
                    | "thread/attachmentOwner/list"
                    | "thread/queue/list"
                    | "model/list"
                    | "account/read"
                    | "config/read"
                    | "configRequirements/read"
            ),
            Self::Respond { .. } => true,
        }
    }
}

pub(crate) fn validate_request(method: &str, params: &Value) -> Result<(), CodexError> {
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

enum Write {
    Message(Message),
    Reply {
        message: Message,
        reply: oneshot::Sender<Result<Value, CodexError>>,
    },
}

#[derive(Serialize)]
struct ThreadState {
    status: Value,
    last_turn: Option<Value>,
    last_assistant: Option<String>,
    updated_at_ms: u64,
}

impl Default for ThreadState {
    fn default() -> Self {
        Self {
            status: json!({"type":"unknown"}),
            last_turn: None,
            last_assistant: None,
            updated_at_ms: now_ms(),
        }
    }
}

struct Cache {
    connection: Value,
    threads: BTreeMap<String, ThreadState>,
    requests: HashMap<RpcId, Value>,
}

impl Default for Cache {
    fn default() -> Self {
        Self {
            connection: json!({"state":"disconnected"}),
            threads: BTreeMap::new(),
            requests: HashMap::new(),
        }
    }
}

impl Cache {
    fn snapshot(&self) -> Value {
        let mut requests: Vec<_> = self.requests.values().collect();
        requests.sort_by_key(|request| request["id"].to_string());
        json!({"connection":self.connection,"threads":self.threads,"pending_requests":requests})
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
        state.updated_at_ms = now_ms();
        if let Some(turns) = thread["turns"].as_array() {
            for turn in turns {
                observe_turn(state, turn);
            }
        }
    }

    fn observe_event(&mut self, event: &Value) {
        let Some(method) = event["method"].as_str() else {
            return;
        };
        if let Some(id) = request_id(event) {
            self.requests.insert(id, event.clone());
        }
        let params = &event["params"];
        if method == "serverRequest/resolved"
            && let Ok(id) = serde_json::from_value::<RpcId>(params["requestId"].clone())
        {
            self.requests.remove(&id);
        }
        if method == "thread/started" {
            self.observe_thread(&params["thread"]);
        }
        let Some(id) = params["threadId"].as_str() else {
            return;
        };
        let state = self.threads.entry(id.into()).or_default();
        state.updated_at_ms = now_ms();
        match method {
            "thread/status/changed" => state.status = params["status"].clone(),
            "turn/started" => {
                state.status = json!({"type":"active","activeFlags":[]});
                state.last_assistant = None;
                observe_turn(state, &params["turn"]);
            }
            "turn/completed" => {
                observe_turn(state, &params["turn"]);
                state.status = json!({"type": match params["turn"]["status"].as_str() {
                    Some("completed" | "interrupted" | "failed") => "idle",
                    _ => "unknown",
                }});
            }
            "item/completed" => observe_assistant(state, &params["item"]),
            _ => {}
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
    retry_observation_at: HashMap<String, Instant>,
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
        let NativeCall::Request { method, params } = &execute.call else {
            let _ = execute.reply.send(Err(rejected(
                "native response must match a pending request",
            )));
            return Ok(());
        };
        let method = method.clone();
        let params = params.clone();
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
                    message: reason.into(),
                }));
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

async fn connection_loop(
    socket: PathBuf,
    mut commands: mpsc::UnboundedReceiver<Execute>,
    observed: watch::Receiver<BTreeSet<String>>,
    cache: Arc<RwLock<Cache>>,
    events: mpsc::UnboundedSender<Value>,
) {
    let mut backoff = 1;
    loop {
        let connect = async {
            let stream = UnixStream::connect(&socket)
                .await
                .map_err(|error| error.to_string())?;
            client_async("ws://localhost/", stream)
                .await
                .map(|(stream, _)| stream)
                .map_err(|error| error.to_string())
        };
        let connect = timeout(Duration::from_secs(10), connect);
        tokio::pin!(connect);
        let result = loop {
            tokio::select! {
                result = &mut connect => break result.map_err(|_| "Codex connection timed out".to_string()).and_then(|result| result),
                execute = commands.recv() => match execute {
                    Some(execute) => { let _ = execute.reply.send(Err(rejected("Codex is disconnected"))); }
                    None => return,
                }
            }
        };
        let reason = match result {
            Ok(stream) => {
                let reason = run_session(stream, &mut commands, &observed, &cache, &events).await;
                if cache.read().await.connection["state"] == "connected" {
                    backoff = 1;
                }
                reason
            }
            Err(reason) => reason,
        };
        cache.write().await.disconnect(&reason);
        let _ = events.send(json!({"method":"bridge.codex_disconnected","params":{"reason":reason,"at_ms":now_ms()}}));
        let delay = tokio::time::sleep(Duration::from_secs(backoff));
        tokio::pin!(delay);
        loop {
            tokio::select! {
                _ = &mut delay => break,
                execute = commands.recv() => match execute {
                    Some(execute) => { let _ = execute.reply.send(Err(rejected("Codex is disconnected"))); }
                    None => return,
                }
            }
        }
        backoff = (backoff * 2).min(30);
    }
}

async fn run_session(
    stream: WebSocketStream<UnixStream>,
    commands: &mut mpsc::UnboundedReceiver<Execute>,
    observed: &watch::Receiver<BTreeSet<String>>,
    cache: &Arc<RwLock<Cache>>,
    events: &mpsc::UnboundedSender<Value>,
) -> String {
    let (mut sink, mut reader) = stream.split();
    let (writer, mut writes) = mpsc::unbounded_channel();
    let writer_task = tokio::spawn(async move {
        while let Some(write) = writes.recv().await {
            let (message, reply) = match write {
                Write::Message(message) => (message, None),
                Write::Reply { message, reply } => (message, Some(reply)),
            };
            let result = timeout(RPC_TIMEOUT, sink.send(message))
                .await
                .map_err(|_| "Codex write timed out".to_string())
                .and_then(|result| result.map_err(|error| error.to_string()));
            if let Some(reply) = reply {
                let result = result
                    .as_ref()
                    .map(|_| json!({"sent":true}))
                    .map_err(|error| CodexError {
                        uncertain: true,
                        message: error.clone(),
                    });
                let _ = reply.send(result);
            }
            result?;
        }
        Ok::<(), String>(())
    });
    let mut writer_task = writer_task;
    let mut state = Session::default();
    let _ = state.request(&writer, "initialize", json!({"clientInfo":{"name":"vibeface","title":"Vibeface Rust Bridge","version":env!("CARGO_PKG_VERSION")},"capabilities":{"experimentalApi":true}}), PendingKind::Initialize, false);
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
            execute = commands.recv() => {
                let Some(execute) = execute else { break "Codex adapter stopped".into() };
                if !state.ready {
                    let _ = execute.reply.send(Err(rejected("Codex is initializing")));
                    continue;
                }
                if let NativeCall::Respond { id: request_id, result: response } = &execute.call {
                    let pending = cache.write().await.requests.remove(request_id);
                    if pending.is_none() {
                        let _ = execute.reply.send(Err(rejected("request is unknown or already resolved")));
                    } else {
                        let message = Message::Text(json!({"id":request_id,"result":response}).to_string().into());
                        if let Err(error) = writer.send(Write::Reply { message, reply:execute.reply }) {
                            if let Write::Reply { reply, .. } = error.0 {
                                let _ = reply.send(Err(rejected("Codex writer closed before replying")));
                            }
                            break "Codex writer stopped".into();
                        }
                    }
                    continue;
                }
                if let Err(reason) = state.command(&writer, execute) { break reason; }
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
                if message["method"].is_string() {
                    cache.write().await.observe_event(&message);
                    if (message["method"] == "thread/closed" || message["method"] == "thread/unsubscribed")
                        && let Some(id) = message["params"]["threadId"].as_str() { state.subscribed.remove(id); }
                    let _ = events.send(message);
                    continue;
                }
                let Some(id) = message.get("id").and_then(|id| serde_json::from_value::<RpcId>(id.clone()).ok()) else { continue };
                let Some(pending) = state.pending.remove(&id) else { continue };
                if let Some(error) = message.get("error") {
                    let initialization = matches!(pending.kind, PendingKind::Initialize);
                    state.fail(pending, &format!("Codex rejected request: {error}"), false);
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
                        let _ = events.send(json!({"method":"bridge.codex_connected","params":{"userAgent":result["userAgent"],"at_ms":now_ms()}}));
                    }
                    PendingKind::ObserveThread(thread_id) => {
                        cache.write().await.observe_thread(&result["thread"]);
                        if !state.subscribed.contains(&thread_id)
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
                        cache.write().await.observe_thread(&result["thread"]);
                        if let NativeCall::Request { method, .. } = &execute.call
                            && matches!(method.as_str(), "thread/start" | "thread/resume")
                            && let Some(id) = result["thread"]["id"].as_str() { state.subscribed.insert(id.into()); }
                        let _ = execute.reply.send(Ok(result));
                    }
                }
            }
            _ = ticker.tick() => {
                if let Err(reason) = state.observe_threads(&writer, &observed.borrow()) { break reason; }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn notifications_and_approvals_flow_while_an_rpc_waits() {
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
            assert_eq!(received_events.recv().await.unwrap()["method"], "bridge.codex_connected");

            let querying = client.clone();
            let response = tokio::spawn(async move {
                querying.request("thread/read", json!({"threadId":"t","includeTurns":false})).await
            });
            let request: Value = serde_json::from_str(server.next().await.unwrap().unwrap().to_text().unwrap()).unwrap();
            assert_eq!(request["method"], "thread/read");
            let approval = json!({"id":27,"method":"item/commandExecution/requestApproval","params":{"threadId":"t"}});
            server.send(Message::Text(approval.to_string().into())).await.unwrap();
            assert_eq!(received_events.recv().await.unwrap(), approval);
            assert_eq!(client.snapshot().await["pending_requests"][0]["id"], 27);
            server.send(Message::Text(json!({"id":request["id"],"result":{"thread":{"id":"t","status":{"type":"idle"}}}}).to_string().into())).await.unwrap();
            assert_eq!(response.await.unwrap().unwrap()["thread"]["id"], "t");

            let replying = client.clone();
            let reply = tokio::spawn(async move {
                replying.respond(json!(27), json!({"decision":"decline"})).await
            });
            let sent: Value = serde_json::from_str(server.next().await.unwrap().unwrap().to_text().unwrap()).unwrap();
            assert_eq!(sent, json!({"id":27,"result":{"decision":"decline"}}));
            reply.await.unwrap().unwrap();
            assert!(client.respond(json!(27), json!({"decision":"decline"})).await.is_err());
            server.close(None).await.unwrap();
            assert_eq!(session.await.unwrap(), "Codex connection closed");
        }).await.expect("mock Codex session must finish");
    }

    #[tokio::test]
    async fn known_threads_resubscribe_after_disconnect_without_replaying_a_turn() {
        timeout(Duration::from_secs(8), async {
            let socket = std::env::temp_dir().join(format!("vibeface-subscribe-{}-{}.sock", std::process::id(), now_ms()));
            let listener = tokio::net::UnixListener::bind(&socket).unwrap();
            let (events, mut received_events) = mpsc::unbounded_channel();
            let client = CodexClient::start(socket.clone(), events);
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
                server.send(Message::Text(json!({"id":turns["id"],"result":{"data":[{"id":"previous-turn","status":"completed","items":[]}]}}).to_string().into())).await.unwrap();
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
                        let event = received_events.recv().await.unwrap();
                        if event == approval || event == completed {
                            recovered.push(event);
                        }
                    }
                    assert_eq!(recovered, vec![approval, completed]);
                    assert_eq!(client.snapshot().await["pending_requests"][0]["id"], 42);
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
            !NativeCall::Request {
                method: "thread/read".into(),
                params: json!({})
            }
            .uncertain()
        );
        assert!(
            NativeCall::Request {
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
    async fn disconnect_fails_pending_mutation_as_uncertain_and_clears_live_state() {
        let mut state = Session::default();
        let (writer, _reader) = mpsc::unbounded_channel();
        let (reply, result) = oneshot::channel();
        state
            .command(
                &writer,
                Execute {
                    call: NativeCall::Request {
                        method: "turn/interrupt".into(),
                        params: json!({"threadId":"t","turnId":"r"}),
                    },
                    reply,
                },
            )
            .unwrap();
        state.subscribed.insert("t".into());
        state.disconnect("socket closed");
        assert!(result.await.unwrap().unwrap_err().uncertain);
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
}
