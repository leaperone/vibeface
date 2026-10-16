use futures_util::{SinkExt, StreamExt};
use serde::Serialize;
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    fmt,
    path::PathBuf,
    sync::Arc,
    time::Duration,
};
use tokio::{
    net::UnixStream,
    sync::{RwLock, mpsc, oneshot},
    time::{Instant, timeout},
};
use tokio_tungstenite::{WebSocketStream, client_async, tungstenite::Message};
use vibeface_protocol::{Command, RpcId, now_ms};

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
}

impl CodexClient {
    pub fn start(socket: PathBuf, events: mpsc::UnboundedSender<Value>) -> Self {
        let (commands, receiver) = mpsc::unbounded_channel();
        let cache = Arc::new(RwLock::new(Cache::default()));
        tokio::spawn(connection_loop(socket, receiver, cache.clone(), events));
        Self { commands, cache }
    }

    pub async fn execute(&self, command: &Command) -> Result<Value, CodexError> {
        command.validate().map_err(rejected)?;
        let (reply, response) = oneshot::channel();
        self.commands
            .send(Execute {
                command: command.clone(),
                reply,
            })
            .map_err(|_| rejected("Codex adapter is stopped"))?;
        response.await.unwrap_or_else(|_| {
            Err(CodexError {
                uncertain: mutates(command),
                message: "Codex adapter stopped before returning a result".into(),
            })
        })
    }

    pub async fn snapshot(&self) -> Value {
        self.cache.read().await.snapshot()
    }
}

struct Execute {
    command: Command,
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

fn mutates(command: &Command) -> bool {
    !matches!(
        command,
        Command::ReadThread { .. } | Command::ListTurns { .. }
    )
}

fn wire_command(command: &Command) -> Option<(&'static str, Value)> {
    Some(match command {
        Command::ReadThread { thread_id } => (
            "thread/read",
            json!({"threadId":thread_id,"includeTurns":false}),
        ),
        Command::ListTurns { thread_id, cursor } => (
            "thread/turns/list",
            json!({"threadId":thread_id,"cursor":cursor,"limit":20,"itemsView":"full"}),
        ),
        Command::StartThread { cwd } => ("thread/start", json!({"cwd":cwd})),
        Command::StartTurn { thread_id, text } => (
            "turn/start",
            json!({"threadId":thread_id,"input":[{"type":"text","text":text,"text_elements":[]}]}),
        ),
        Command::SteerTurn {
            thread_id,
            turn_id,
            text,
        } => (
            "turn/steer",
            json!({"threadId":thread_id,"expectedTurnId":turn_id,"input":[{"type":"text","text":text,"text_elements":[]}]}),
        ),
        Command::InterruptTurn { thread_id, turn_id } => (
            "turn/interrupt",
            json!({"threadId":thread_id,"turnId":turn_id}),
        ),
        Command::RespondRequest { .. } => return None,
    })
}

enum PendingKind {
    Initialize,
    Command(Execute),
    ResumeForTurn(String),
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
    resuming: HashMap<String, Vec<Execute>>,
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
        let Some((method, params)) = wire_command(&execute.command) else {
            let _ = execute
                .reply
                .send(Err(rejected("command is not handled by the Codex adapter")));
            return Ok(());
        };
        let uncertain = mutates(&execute.command);
        self.request(
            writer,
            method,
            params,
            PendingKind::Command(execute),
            uncertain,
        )
    }

    fn fail(&mut self, pending: Pending, reason: &str, uncertain: bool) {
        match pending.kind {
            PendingKind::Command(execute) => {
                let _ = execute.reply.send(Err(CodexError {
                    uncertain: uncertain && pending.uncertain,
                    message: reason.into(),
                }));
            }
            PendingKind::ResumeForTurn(thread) => {
                if let Some(waiters) = self.resuming.remove(&thread) {
                    for execute in waiters {
                        let _ = execute.reply.send(Err(rejected(format!(
                            "could not subscribe to thread before starting turn: {reason}"
                        ))));
                    }
                }
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
                let reason = run_session(stream, &mut commands, &cache, &events).await;
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
    let reason = 'session: loop {
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
                if let Command::RespondRequest { request_id, response } = &execute.command {
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
                if let Command::StartTurn { thread_id, .. } = &execute.command
                    && !state.subscribed.contains(thread_id) {
                        let thread_id = thread_id.clone();
                        let waiters = state.resuming.entry(thread_id.clone()).or_default();
                        waiters.push(execute);
                        if waiters.len() == 1
                            && let Err(reason) = state.request(&writer, "thread/resume", json!({"threadId":thread_id,"excludeTurns":true}), PendingKind::ResumeForTurn(thread_id), false) {
                                break reason;
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
                    PendingKind::ResumeForTurn(thread_id) => {
                        cache.write().await.observe_thread(&result["thread"]);
                        state.subscribed.insert(thread_id.clone());
                        for execute in state.resuming.remove(&thread_id).unwrap_or_default() {
                            if let Err(reason) = state.command(&writer, execute) { break 'session reason; }
                        }
                    }
                    PendingKind::Command(execute) => {
                        cache.write().await.observe_thread(&result["thread"]);
                        if matches!(execute.command, Command::StartThread { .. })
                            && let Some(id) = result["thread"]["id"].as_str() { state.subscribed.insert(id.into()); }
                        let _ = execute.reply.send(Ok(result));
                    }
                }
            }
            _ = ticker.tick() => {
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
            let client = CodexClient { commands, cache: cache.clone() };
            let session = tokio::spawn(async move {
                run_session(websocket, &mut receiver, &cache, &events).await
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
                querying.execute(&Command::ReadThread { thread_id:"t".into() }).await
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
                replying.execute(&Command::RespondRequest { request_id:RpcId::Number(27), response:json!({"decision":"decline"}) }).await
            });
            let sent: Value = serde_json::from_str(server.next().await.unwrap().unwrap().to_text().unwrap()).unwrap();
            assert_eq!(sent, json!({"id":27,"result":{"decision":"decline"}}));
            assert_eq!(reply.await.unwrap().unwrap(), json!({"sent":true}));
            assert!(client.execute(&Command::RespondRequest { request_id:RpcId::Number(27), response:json!({"decision":"decline"}) }).await.is_err());
            server.close(None).await.unwrap();
            assert_eq!(session.await.unwrap(), "Codex connection closed");
        }).await.expect("mock Codex session must finish");
    }

    #[test]
    fn command_fields_match_codex_schema() {
        let (method, params) = wire_command(&Command::SteerTurn {
            thread_id: "thread".into(),
            turn_id: "turn".into(),
            text: "continue".into(),
        })
        .unwrap();
        assert_eq!(method, "turn/steer");
        assert_eq!(
            params,
            json!({"threadId":"thread","expectedTurnId":"turn","input":[{"type":"text","text":"continue","text_elements":[]}]})
        );
        assert_eq!(
            wire_command(&Command::ListTurns {
                thread_id: "thread".into(),
                cursor: Some("next".into())
            })
            .unwrap()
            .1,
            json!({"threadId":"thread","cursor":"next","limit":20,"itemsView":"full"})
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
                    command: Command::InterruptTurn {
                        thread_id: "t".into(),
                        turn_id: "r".into(),
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
