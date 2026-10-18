use futures_util::{Sink, SinkExt, Stream, StreamExt};
use serde::Serialize;
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet, HashMap, HashSet},
    fmt, io,
    path::PathBuf,
    pin::Pin,
    process::Stdio,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{
    io::{AsyncBufRead, AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::UnixStream,
    sync::{RwLock, mpsc, oneshot, watch},
    time::{Instant, timeout},
};
use tokio_tungstenite::{client_async, tungstenite::Message};
use vibeface_protocol::{Command, RpcId, now_ms};

const RPC_TIMEOUT: Duration = Duration::from_secs(60);
const MAX_STDIO_LINE_BYTES: usize = 1_048_576;

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

    pub fn start_process(
        command: tokio::process::Command,
        events: mpsc::UnboundedSender<Value>,
    ) -> Self {
        let (commands, receiver) = mpsc::unbounded_channel();
        let cache = Arc::new(RwLock::new(Cache::default()));
        let (observed_threads, observed) = watch::channel(BTreeSet::new());
        tokio::spawn(process_session(
            command,
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

    pub async fn execute(&self, command: &Command) -> Result<Value, CodexError> {
        command.validate().map_err(rejected)?;
        let cache = self.cache.read().await;
        if cache.connection["state"] != "connected" {
            return Err(rejected("Codex is disconnected"));
        }
        let generation = cache.generation;
        drop(cache);
        let (reply, response) = oneshot::channel();
        self.commands
            .send(Execute {
                command: command.clone(),
                reply: Reply::new(reply),
                generation,
            })
            .map_err(|_| rejected("Codex adapter is stopped"))?;
        response.await.unwrap_or_else(|_| {
            Err(CodexError {
                uncertain: mutates(command),
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

    pub fn is_stopped(&self) -> bool {
        self.commands.is_closed()
    }
}

struct Execute {
    command: Command,
    reply: Reply,
    generation: u64,
}

#[derive(Clone)]
#[allow(clippy::type_complexity)]
struct Reply(Arc<Mutex<Option<oneshot::Sender<Result<Value, CodexError>>>>>);

impl Reply {
    fn new(reply: oneshot::Sender<Result<Value, CodexError>>) -> Self {
        Self(Arc::new(Mutex::new(Some(reply))))
    }

    fn is_closed(&self) -> bool {
        self.0
            .lock()
            .unwrap()
            .as_ref()
            .is_none_or(|reply| reply.is_closed())
    }

    async fn closed(&self) {
        futures_util::future::poll_fn(|cx| match self.0.lock().unwrap().as_mut() {
            Some(reply) => reply.poll_closed(cx),
            None => std::task::Poll::Ready(()),
        })
        .await
    }

    fn send(&self, value: Result<Value, CodexError>) -> Result<(), Result<Value, CodexError>> {
        match self.0.lock().unwrap().take() {
            Some(reply) => reply.send(value),
            None => Err(value),
        }
    }
}

enum Write {
    Message(Message),
    Request {
        message: Message,
        reply: Reply,
        deadline: Instant,
    },
    Reply {
        message: Message,
        reply: Reply,
        request_id: RpcId,
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
    generation: u64,
    connection: Value,
    threads: BTreeMap<String, ThreadState>,
    requests: HashMap<RpcId, Value>,
    replying: HashSet<RpcId>,
}

impl Default for Cache {
    fn default() -> Self {
        Self {
            generation: 0,
            connection: json!({"state":"disconnected"}),
            threads: BTreeMap::new(),
            requests: HashMap::new(),
            replying: HashSet::new(),
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
        self.generation += 1;
        self.connection = json!({"state":"disconnected","error":reason,"at_ms":now_ms()});
        self.requests.clear();
        self.replying.clear();
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
            self.replying.remove(&id);
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
    SubscribeThread(String),
}

struct Pending {
    kind: PendingKind,
    deadline: Instant,
    uncertain: bool,
}

#[derive(Default)]
struct Session {
    generation: u64,
    next_id: u64,
    ready: bool,
    pending: HashMap<RpcId, Pending>,
    subscribed: HashSet<String>,
    resuming: HashMap<String, Vec<Execute>>,
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
        let message = Message::Text(
            json!({"id":id,"method":method,"params":params})
                .to_string()
                .into(),
        );
        let deadline = Instant::now() + RPC_TIMEOUT;
        let write = match &kind {
            PendingKind::Command(execute) => Write::Request {
                message,
                reply: execute.reply.clone(),
                deadline,
            },
            _ => Write::Message(message),
        };
        self.pending.insert(
            id,
            Pending {
                kind,
                uncertain,
                deadline,
            },
        );
        writer
            .send(write)
            .map_err(|_| "Codex writer is closed".into())
    }

    fn command(
        &mut self,
        writer: &mpsc::UnboundedSender<Write>,
        execute: Execute,
    ) -> Result<(), String> {
        if execute.reply.is_closed() {
            return Ok(());
        }
        if execute.generation != self.generation {
            let _ = execute.reply.send(Err(rejected(
                "Codex connection changed before sending command",
            )));
            return Ok(());
        }
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
            if self.subscribed.contains(id)
                || self.resuming.contains_key(id)
                || self
                    .retry_observation_at
                    .get(id)
                    .is_some_and(|at| *at > Instant::now())
            {
                continue;
            }
            self.resuming.insert(id.clone(), Vec::new());
            self.request(
                writer,
                "thread/resume",
                json!({"threadId":id,"excludeTurns":true}),
                PendingKind::SubscribeThread(id.clone()),
                false,
            )?;
        }
        Ok(())
    }

    fn fail(&mut self, pending: Pending, reason: &str, uncertain: bool) {
        match pending.kind {
            PendingKind::Command(execute) => {
                let _ = execute.reply.send(Err(CodexError {
                    uncertain: uncertain && pending.uncertain,
                    message: reason.into(),
                }));
            }
            PendingKind::SubscribeThread(thread) => {
                self.retry_observation_at
                    .insert(thread.clone(), Instant::now() + Duration::from_secs(5));
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
                let (sink, reader) = stream.split();
                let reason =
                    run_transport(sink, reader, &mut commands, &observed, &cache, &events).await;
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

async fn process_session(
    mut command: tokio::process::Command,
    mut commands: mpsc::UnboundedReceiver<Execute>,
    observed: watch::Receiver<BTreeSet<String>>,
    cache: Arc<RwLock<Cache>>,
    events: mpsc::UnboundedSender<Value>,
) {
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    let reason = match command.spawn() {
        Ok(mut child) => {
            let stdin = child.stdin.take().expect("piped Codex stdin");
            let stdout = child.stdout.take().expect("piped Codex stdout");
            let sink = Box::pin(futures_util::sink::unfold(
                stdin,
                |mut stdin, message| async move {
                    let Message::Text(text) = message else {
                        return Err(io::Error::other("unsupported Codex stdio frame"));
                    };
                    if text.len() > MAX_STDIO_LINE_BYTES || text.contains(['\n', '\r']) {
                        return Err(io::Error::other("invalid Codex stdio line"));
                    }
                    stdin.write_all(text.as_bytes()).await?;
                    stdin.write_all(b"\n").await?;
                    stdin.flush().await?;
                    Ok(stdin)
                },
            ));
            let reader = Box::pin(futures_util::stream::try_unfold(
                BufReader::new(stdout),
                |mut reader| async move {
                    Ok::<_, io::Error>(
                        read_stdio_line(&mut reader)
                            .await?
                            .map(|line| (Message::Text(line.into()), reader)),
                    )
                },
            ));
            let reason =
                run_transport(sink, reader, &mut commands, &observed, &cache, &events).await;
            let _ = child.kill().await;
            let _ = child.wait().await;
            reason
        }
        Err(_) => "Codex process failed to start".into(),
    };
    commands.close();
    while let Ok(execute) = commands.try_recv() {
        let _ = execute.reply.send(Err(rejected(&reason)));
    }
    cache.write().await.disconnect(&reason);
    let _ = events.send(
        json!({"method":"bridge.codex_disconnected","params":{"reason":reason,"at_ms":now_ms()}}),
    );
}

async fn read_stdio_line<R: AsyncBufRead + Unpin>(reader: &mut R) -> io::Result<Option<String>> {
    let mut line = Vec::new();
    loop {
        let available = reader.fill_buf().await?;
        if available.is_empty() {
            return if line.is_empty() {
                Ok(None)
            } else {
                Err(io::Error::other("truncated Codex stdio line"))
            };
        }
        let length = available
            .iter()
            .position(|byte| *byte == b'\n')
            .map_or(available.len(), |end| end + 1);
        if line.len() + length > MAX_STDIO_LINE_BYTES + 1 {
            return Err(io::Error::other("Codex stdio line is too large"));
        }
        line.extend_from_slice(&available[..length]);
        let complete = line.last() == Some(&b'\n');
        reader.consume(length);
        if complete {
            line.pop();
            if line.last() == Some(&b'\r') {
                line.pop();
            }
            return String::from_utf8(line)
                .map(Some)
                .map_err(|_| io::Error::other("Codex stdio line is not UTF-8"));
        }
    }
}

async fn write_messages<S>(
    mut sink: S,
    mut writes: mpsc::UnboundedReceiver<Write>,
    writer_cache: Arc<RwLock<Cache>>,
) -> Result<(), String>
where
    S: Sink<Message> + Unpin,
    S::Error: fmt::Display,
{
    while let Some(write) = writes.recv().await {
        let (message, reply, cancelled, deadline, approval) = match write {
            Write::Message(message) => (message, None, None, None, None),
            Write::Request {
                message,
                reply,
                deadline,
            } => {
                if reply.is_closed() {
                    continue;
                }
                if deadline <= Instant::now() {
                    let _ = reply.send(Err(rejected("Codex request expired before being written")));
                    continue;
                }
                (message, None, Some(reply), Some(deadline), None)
            }
            Write::Reply {
                message,
                reply,
                request_id,
            } => (
                message,
                Some(reply.clone()),
                Some(reply),
                None,
                Some(request_id),
            ),
        };
        if cancelled.as_ref().is_some_and(Reply::is_closed) {
            if let Some(id) = approval {
                writer_cache.write().await.replying.remove(&id);
            }
            continue;
        }
        let write = async {
            futures_util::future::poll_fn(|cx| Pin::new(&mut sink).poll_ready(cx))
                .await
                .map_err(|error| error.to_string())?;
            if cancelled.as_ref().is_some_and(Reply::is_closed) {
                return Ok(false);
            }
            if deadline.is_some_and(|deadline| deadline <= Instant::now()) {
                if let Some(reply) = &cancelled {
                    let _ = reply.send(Err(rejected("Codex request expired before being written")));
                }
                return Ok(false);
            }
            Pin::new(&mut sink)
                .start_send(message)
                .map_err(|error| error.to_string())?;
            sink.flush().await.map_err(|error| error.to_string())?;
            Ok(true)
        };
        let duration = deadline.map_or(RPC_TIMEOUT, |deadline| {
            deadline.saturating_duration_since(Instant::now())
        });
        let result = timeout(duration, async {
            tokio::select! {
                biased;
                _ = async { match &cancelled { Some(reply) => reply.closed().await, None => std::future::pending::<()>().await } } => Err("Codex request cancelled during write; outcome is unknown".to_string()),
                result = write => result,
            }
        })
        .await
        .map_err(|_| "Codex write timed out".to_string())
        .and_then(|result| result);
        if let Some(id) = approval {
            let mut cache = writer_cache.write().await;
            cache.replying.remove(&id);
            if matches!(result, Ok(true)) {
                cache.requests.remove(&id);
            }
        }
        if matches!(result, Ok(false)) {
            continue;
        }
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
}

async fn run_transport<S, R, E>(
    sink: S,
    mut reader: R,
    commands: &mut mpsc::UnboundedReceiver<Execute>,
    observed: &watch::Receiver<BTreeSet<String>>,
    cache: &Arc<RwLock<Cache>>,
    events: &mpsc::UnboundedSender<Value>,
) -> String
where
    S: Sink<Message> + Unpin + Send + 'static,
    S::Error: fmt::Display,
    R: Stream<Item = Result<Message, E>> + Unpin,
    E: fmt::Display,
{
    let (writer, writes) = mpsc::unbounded_channel();
    let writer_task = tokio::spawn(write_messages(sink, writes, cache.clone()));
    let mut writer_task = writer_task;
    let mut state = Session {
        generation: cache.read().await.generation,
        ..Session::default()
    };
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
                if execute.reply.is_closed() { continue; }
                if execute.generation != state.generation {
                    let _ = execute.reply.send(Err(rejected("Codex connection changed before sending command")));
                    continue;
                }
                if !state.ready {
                    let _ = execute.reply.send(Err(rejected("Codex is initializing")));
                    continue;
                }
                if let Command::RespondRequest { request_id, response } = &execute.command {
                    let mut request_cache = cache.write().await;
                    let pending = request_cache.requests.contains_key(request_id) && request_cache.replying.insert(request_id.clone());
                    drop(request_cache);
                    if !pending {
                        let _ = execute.reply.send(Err(rejected("request is unknown or already resolved")));
                    } else {
                        let message = Message::Text(json!({"id":request_id,"result":response}).to_string().into());
                        if let Err(error) = writer.send(Write::Reply { message, reply:execute.reply, request_id:request_id.clone() }) {
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
                        let subscribing = state.resuming.contains_key(&thread_id);
                        state.resuming.entry(thread_id.clone()).or_default().push(execute);
                        if !subscribing
                            && let Err(reason) = state.request(&writer, "thread/resume", json!({"threadId":thread_id,"excludeTurns":true}), PendingKind::SubscribeThread(thread_id), false) {
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
                    PendingKind::SubscribeThread(thread_id) => {
                        cache.write().await.observe_thread(&result["thread"]);
                        state.subscribed.insert(thread_id.clone());
                        state.retry_observation_at.remove(&thread_id);
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
    async fn stdio_lines_are_bounded_and_require_complete_utf8_frames() {
        let mut fragmented = BufReader::with_capacity(2, &b"{\"id\":1}\r\n"[..]);
        assert_eq!(
            read_stdio_line(&mut fragmented).await.unwrap().as_deref(),
            Some("{\"id\":1}")
        );
        assert!(read_stdio_line(&mut fragmented).await.unwrap().is_none());
        let mut boundary = vec![b' '; MAX_STDIO_LINE_BYTES];
        boundary.push(b'\n');
        assert_eq!(
            read_stdio_line(&mut &boundary[..])
                .await
                .unwrap()
                .unwrap()
                .len(),
            MAX_STDIO_LINE_BYTES
        );
        assert!(
            read_stdio_line(&mut &vec![b'x'; MAX_STDIO_LINE_BYTES + 2][..])
                .await
                .is_err()
        );
        assert!(read_stdio_line(&mut &b"{\"id\":1}"[..]).await.is_err());
        assert!(read_stdio_line(&mut &b"\xff\n"[..]).await.is_err());
    }

    #[tokio::test]
    async fn cancelled_and_expired_writes_never_reach_the_transport() {
        let (writer, writes) = mpsc::unbounded_channel();
        let (output, mut received) = mpsc::unbounded_channel();
        let cache = Arc::new(RwLock::new(Cache::default()));
        let sink = Box::pin(futures_util::sink::unfold(
            output,
            |output, message| async move {
                output.send(message).unwrap();
                Ok::<_, io::Error>(output)
            },
        ));
        let (cancelled, response) = oneshot::channel();
        drop(response);
        writer
            .send(Write::Request {
                message: Message::Text("cancelled".into()),
                reply: Reply::new(cancelled),
                deadline: Instant::now() + RPC_TIMEOUT,
            })
            .unwrap();
        let request_id = RpcId::Number(27);
        {
            let mut cache = cache.write().await;
            cache
                .requests
                .insert(request_id.clone(), json!({"id":27,"method":"approval"}));
            cache.replying.insert(request_id.clone());
        }
        let (reply, approval_response) = oneshot::channel();
        drop(approval_response);
        writer
            .send(Write::Reply {
                message: Message::Text("cancelled approval".into()),
                reply: Reply::new(reply),
                request_id: request_id.clone(),
            })
            .unwrap();
        let (expired, response) = oneshot::channel();
        writer
            .send(Write::Request {
                message: Message::Text("expired".into()),
                reply: Reply::new(expired),
                deadline: Instant::now() - Duration::from_secs(1),
            })
            .unwrap();
        writer
            .send(Write::Message(Message::Text("live".into())))
            .unwrap();
        drop(writer);
        write_messages(sink, writes, cache.clone()).await.unwrap();
        assert!(!response.await.unwrap().unwrap_err().uncertain);
        assert_eq!(received.recv().await.unwrap(), Message::Text("live".into()));
        assert!(received.recv().await.is_none());
        let cache = cache.read().await;
        assert!(cache.requests.contains_key(&request_id));
        assert!(!cache.replying.contains(&request_id));
    }

    #[tokio::test]
    async fn cancelling_a_backpressured_write_closes_it_before_late_delivery() {
        timeout(Duration::from_secs(3), async {
            let (writer, writes) = mpsc::unbounded_channel();
            let (started, mut started_rx) = mpsc::unbounded_channel();
            let (output, mut received) = mpsc::unbounded_channel();
            let (release, gate) = oneshot::channel();
            let sink = Box::pin(futures_util::sink::unfold(
                Some(gate),
                move |mut gate, message| {
                    let started = started.clone();
                    let output = output.clone();
                    async move {
                        started.send(()).unwrap();
                        if let Some(gate) = gate.take() {
                            let _ = gate.await;
                        }
                        output.send(message).unwrap();
                        Ok::<_, io::Error>(gate)
                    }
                },
            ));
            let (reply, response) = oneshot::channel();
            writer
                .send(Write::Request {
                    message: Message::Text("mutation".into()),
                    reply: Reply::new(reply),
                    deadline: Instant::now() + RPC_TIMEOUT,
                })
                .unwrap();
            let task = tokio::spawn(write_messages(
                sink,
                writes,
                Arc::new(RwLock::new(Cache::default())),
            ));
            started_rx.recv().await.unwrap();
            drop(response);
            assert!(
                task.await
                    .unwrap()
                    .unwrap_err()
                    .contains("cancelled during write")
            );
            assert!(release.send(()).is_err());
            assert!(received.recv().await.is_none());
        })
        .await
        .expect("cancelled write must terminate");
    }

    #[tokio::test]
    async fn stale_generation_commands_are_rejected_before_writing() {
        let mut session = Session {
            generation: 1,
            ..Session::default()
        };
        let (writer, mut writes) = mpsc::unbounded_channel();
        let (reply, response) = oneshot::channel();
        session
            .command(
                &writer,
                Execute {
                    command: Command::InterruptTurn {
                        thread_id: "t".into(),
                        turn_id: "r".into(),
                    },
                    reply: Reply::new(reply),
                    generation: 0,
                },
            )
            .unwrap();
        assert!(!response.await.unwrap().unwrap_err().uncertain);
        assert!(writes.try_recv().is_err());
        assert!(session.pending.is_empty());
    }

    #[tokio::test]
    async fn cancelled_resume_waiters_are_not_sent_after_subscription() {
        timeout(Duration::from_secs(3), async {
            let (left, right) = UnixStream::pair().unwrap();
            let (websocket, server) = tokio::join!(
                client_async("ws://localhost/", left),
                tokio_tungstenite::accept_async(right)
            );
            let websocket = websocket.unwrap().0;
            let (commands, mut receiver) = mpsc::unbounded_channel();
            let (events, mut received) = mpsc::unbounded_channel();
            let cache = Arc::new(RwLock::new(Cache::default()));
            let (observed_threads, observed) = watch::channel(BTreeSet::new());
            let client = CodexClient {
                commands,
                cache: cache.clone(),
                observed_threads,
            };
            let task = tokio::spawn(async move {
                let (sink, reader) = websocket.split();
                run_transport(sink, reader, &mut receiver, &observed, &cache, &events).await
            });
            let mut server = server.unwrap();
            let initialize: Value =
                serde_json::from_str(server.next().await.unwrap().unwrap().to_text().unwrap())
                    .unwrap();
            server
                .send(Message::Text(
                    json!({"id":initialize["id"],"result":{}})
                        .to_string()
                        .into(),
                ))
                .await
                .unwrap();
            assert_eq!(
                received.recv().await.unwrap()["method"],
                "bridge.codex_connected"
            );
            let initialized: Value =
                serde_json::from_str(server.next().await.unwrap().unwrap().to_text().unwrap())
                    .unwrap();
            assert_eq!(initialized["method"], "initialized");
            let starting = client.clone();
            let send = tokio::spawn(async move {
                starting
                    .execute(&Command::StartTurn {
                        thread_id: "t".into(),
                        text: "cancelled".into(),
                    })
                    .await
            });
            let resume: Value =
                serde_json::from_str(server.next().await.unwrap().unwrap().to_text().unwrap())
                    .unwrap();
            assert_eq!(resume["method"], "thread/resume");
            send.abort();
            assert!(send.await.unwrap_err().is_cancelled());
            server
                .send(Message::Text(
                    json!({"id":resume["id"],"result":{"thread":{"id":"t"}}})
                        .to_string()
                        .into(),
                ))
                .await
                .unwrap();
            let querying = client.clone();
            let read = tokio::spawn(async move {
                querying
                    .execute(&Command::ReadThread {
                        thread_id: "t".into(),
                    })
                    .await
            });
            let request: Value =
                serde_json::from_str(server.next().await.unwrap().unwrap().to_text().unwrap())
                    .unwrap();
            assert_eq!(request["method"], "thread/read");
            server
                .send(Message::Text(
                    json!({"id":request["id"],"result":{"thread":{"id":"t"}}})
                        .to_string()
                        .into(),
                ))
                .await
                .unwrap();
            assert_eq!(read.await.unwrap().unwrap()["thread"]["id"], "t");
            server.close(None).await.unwrap();
            assert_eq!(task.await.unwrap(), "Codex connection closed");
        })
        .await
        .expect("cancelled resume waiter must terminate");
    }

    #[tokio::test]
    async fn stdio_process_keeps_approvals_manual_and_stays_closed_after_eof() {
        timeout(Duration::from_secs(3), async {
            let mut command = tokio::process::Command::new("sh");
            command.args([
                "-c",
                r#"
IFS= read -r initialize || exit 1
printf '%s\n' '{"id":"vibeface-1","result":{"userAgent":"stdio-mock"}}'
IFS= read -r initialized || exit 1
IFS= read -r request || exit 1
printf '%s\n' '{"id":27,"method":"item/commandExecution/requestApproval","params":{"threadId":"t"}}'
IFS= read -r reply || exit 1
test "$reply" = '{"id":27,"result":{"decision":"decline"}}' || exit 2
printf '%s\n' '{"id":"vibeface-2","result":{"thread":{"id":"t","status":{"type":"idle"}}}}'
"#,
            ]);
            let (events, mut received) = mpsc::unbounded_channel();
            let client = CodexClient::start_process(command, events);
            assert_eq!(
                received.recv().await.unwrap()["method"],
                "bridge.codex_connected"
            );
            let querying = client.clone();
            let response = tokio::spawn(async move {
                querying
                    .execute(&Command::ReadThread {
                        thread_id: "t".into(),
                    })
                    .await
            });
            assert_eq!(
                received.recv().await.unwrap()["method"],
                "item/commandExecution/requestApproval"
            );
            assert_eq!(client.snapshot().await["pending_requests"][0]["id"], 27);
            assert_eq!(
                client
                    .execute(&Command::RespondRequest {
                        request_id: RpcId::Number(27),
                        response: json!({"decision":"decline"})
                    })
                    .await
                    .unwrap(),
                json!({"sent":true})
            );
            assert_eq!(response.await.unwrap().unwrap()["thread"]["id"], "t");
            assert_eq!(
                received.recv().await.unwrap()["method"],
                "bridge.codex_disconnected"
            );
            assert!(
                !client
                    .execute(&Command::StartThread {
                        cwd: "/work".into()
                    })
                    .await
                    .unwrap_err()
                    .uncertain
            );
            assert!(client.is_stopped());
        })
        .await
        .expect("mock stdio process must finish");
    }

    #[tokio::test]
    async fn failed_process_start_rejects_commands_without_retrying() {
        let (events, mut received) = mpsc::unbounded_channel();
        let client = CodexClient::start_process(
            tokio::process::Command::new("/nonexistent/vibeface-codex-process"),
            events,
        );
        let event = timeout(Duration::from_secs(3), received.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(event["method"], "bridge.codex_disconnected");
        assert_eq!(event["params"]["reason"], "Codex process failed to start");
        assert!(client.is_stopped());
        assert!(
            !client
                .execute(&Command::StartThread {
                    cwd: "/work".into()
                })
                .await
                .unwrap_err()
                .uncertain
        );
    }

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
                let (sink, reader) = websocket.split();
                run_transport(sink, reader, &mut receiver, &observed, &cache, &events).await
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

    #[tokio::test]
    async fn known_threads_resubscribe_after_disconnect_without_replaying_a_turn() {
        timeout(Duration::from_secs(8), async {
            let socket = std::env::temp_dir().join(format!("vibeface-subscribe-{}-{}.sock", std::process::id(), now_ms()));
            let listener = tokio::net::UnixListener::bind(&socket).unwrap();
            let (events, mut received_events) = mpsc::unbounded_channel();
            let client = CodexClient::start(socket.clone(), events);
            client.set_observed_threads(vec!["known".into()]).unwrap();
            let mut sent_turns = 0;
            for connection in 0..2 {
                let (stream, _) = listener.accept().await.unwrap();
                let mut server = tokio_tungstenite::accept_async(stream).await.unwrap();
                let initialize: Value = serde_json::from_str(server.next().await.unwrap().unwrap().to_text().unwrap()).unwrap();
                assert_eq!(initialize["method"], "initialize");
                server.send(Message::Text(json!({"id":initialize["id"],"result":{"userAgent":"test"}}).to_string().into())).await.unwrap();
                let initialized: Value = serde_json::from_str(server.next().await.unwrap().unwrap().to_text().unwrap()).unwrap();
                assert_eq!(initialized["method"], "initialized");
                let resume: Value = serde_json::from_str(server.next().await.unwrap().unwrap().to_text().unwrap()).unwrap();
                assert_eq!(resume["method"], "thread/resume");
                assert_eq!(resume["params"], json!({"threadId":"known","excludeTurns":true}));
                server.send(Message::Text(json!({"id":resume["id"],"result":{"thread":{"id":"known","status":{"type":"active"}}}}).to_string().into())).await.unwrap();
                if connection == 0 {
                    let sender = client.clone();
                    let sending = tokio::spawn(async move {
                        sender.execute(&Command::StartTurn { thread_id:"known".into(), text:"write once".into() }).await
                    });
                    let start: Value = serde_json::from_str(server.next().await.unwrap().unwrap().to_text().unwrap()).unwrap();
                    assert_eq!(start["method"], "turn/start");
                    sent_turns += 1;
                    server.close(None).await.unwrap();
                    assert!(sending.await.unwrap().unwrap_err().uncertain);
                } else {
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
                }
            }
            assert_eq!(sent_turns, 1);
            drop(client);
            std::fs::remove_file(socket).unwrap();
        }).await.expect("Codex reconnect must restore observation without replaying a mutation");
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
                    reply: Reply::new(reply),
                    generation: 0,
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
