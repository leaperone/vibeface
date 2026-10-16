mod store;

use anyhow::{Context, Result, bail};
use axum::{
    Json, Router,
    extract::{
        DefaultBodyLimit, Path, Query, State, WebSocketUpgrade,
        ws::{Message, WebSocket},
    },
    http::{HeaderMap, StatusCode, header},
    middleware::{self, Next},
    response::{
        IntoResponse, Response, Sse,
        sse::{Event, KeepAlive},
    },
    routing::{get, post},
};
use clap::{Args, Parser, Subcommand};
use futures_util::{SinkExt, StreamExt, stream};
use serde::Deserialize;
use serde_json::json;
use std::{
    convert::Infallible,
    io::{self, Write},
    net::SocketAddr,
    path::PathBuf,
    sync::{Arc, Mutex, MutexGuard},
    time::{Duration, Instant},
};
use store::Store;
use tokio::sync::{OwnedSemaphorePermit, Semaphore, broadcast, mpsc};
use vibeface_protocol::{
    CommandRecord, CommandRequest, DeviceStatus, ToBridge, ToServer, VERSION, validate_id,
};

const MAX_MESSAGE: usize = 8 * 1_048_576;
const HEARTBEAT_TIMEOUT: Duration = Duration::from_secs(45);

#[derive(Parser)]
#[command(about = "Vibeface device relay and authenticated HTTP client")]
struct Cli {
    #[command(subcommand)]
    command: CliCommand,
}

#[derive(Subcommand)]
enum CliCommand {
    Serve(Serve),
    Client(Client),
}

#[derive(Args)]
struct Serve {
    #[arg(long, default_value = "127.0.0.1:8787")]
    listen: SocketAddr,
    #[arg(long, default_value = "mac-mini")]
    device_id: String,
    #[arg(long, default_value = "server.sqlite3")]
    db: PathBuf,
    #[arg(long, env = "VIBEFACE_BRIDGE_TOKEN", hide_env_values = true)]
    bridge_token: String,
    #[arg(long, env = "VIBEFACE_CLIENT_TOKEN", hide_env_values = true)]
    client_token: String,
}

#[derive(Args)]
struct Client {
    #[arg(long, default_value = "http://127.0.0.1:8787")]
    url: String,
    #[arg(long, default_value = "mac-mini")]
    device_id: String,
    #[arg(long, env = "VIBEFACE_CLIENT_TOKEN", hide_env_values = true)]
    token: String,
    #[command(subcommand)]
    action: ClientAction,
}

#[derive(Subcommand)]
enum ClientAction {
    Devices,
    /// Read a CommandRequest JSON file. Use - for stdin.
    Submit {
        file: PathBuf,
    },
    Command {
        id: String,
    },
    Events {
        #[arg(long, default_value_t = 0)]
        after: u64,
        #[arg(long, default_value_t = 100)]
        limit: usize,
    },
    /// Print the SSE stream. Reconnect using the last received event id.
    Stream {
        #[arg(long, default_value_t = 0)]
        after: u64,
    },
}

#[derive(Clone)]
struct App {
    inner: Arc<Mutex<Core>>,
    device_id: Arc<str>,
    bridge_token: Arc<str>,
    client_token: Arc<str>,
    events: broadcast::Sender<u64>,
    streams: Arc<Semaphore>,
    bridge_slots: Arc<Semaphore>,
}

struct Core {
    store: Store,
    connection: Option<Connection>,
    generation: u64,
}

struct Connection {
    generation: u64,
    sender: mpsc::Sender<ToBridge>,
    heartbeat: Instant,
}

impl Core {
    fn online(&self) -> bool {
        self.connection
            .as_ref()
            .is_some_and(|c| c.heartbeat.elapsed() < HEARTBEAT_TIMEOUT && !c.sender.is_closed())
    }
}

impl App {
    fn core(&self) -> ApiResult<MutexGuard<'_, Core>> {
        self.inner
            .lock()
            .map_err(|_| ApiError::internal("state lock poisoned"))
    }

    fn device(&self, device: &str) -> ApiResult<()> {
        if device == self.device_id.as_ref() {
            Ok(())
        } else {
            Err(ApiError(StatusCode::NOT_FOUND, "device not found".into()))
        }
    }
}

struct ApiError(StatusCode, String);
type ApiResult<T> = std::result::Result<T, ApiError>;

impl ApiError {
    fn internal(error: impl std::fmt::Display) -> Self {
        tracing::error!(%error, "server operation failed");
        Self(
            StatusCode::INTERNAL_SERVER_ERROR,
            "server operation failed".into(),
        )
    }
    fn bad(error: impl std::fmt::Display) -> Self {
        Self(StatusCode::BAD_REQUEST, error.to_string())
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.0, Json(json!({"error":self.1}))).into_response()
    }
}

impl From<anyhow::Error> for ApiError {
    fn from(error: anyhow::Error) -> Self {
        Self::internal(error)
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_writer(io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();
    match Cli::parse().command {
        CliCommand::Serve(args) => serve(args).await,
        CliCommand::Client(args) => client(args).await,
    }
}

async fn serve(args: Serve) -> Result<()> {
    validate_id(&args.device_id).map_err(anyhow::Error::msg)?;
    for token in [&args.bridge_token, &args.client_token] {
        if token.trim().is_empty()
            || token.trim() != token
            || token.contains('\r')
            || token.contains('\n')
        {
            bail!("tokens must be nonempty and contain no surrounding whitespace or newlines");
        }
    }
    if args.bridge_token == args.client_token {
        bail!("bridge and client tokens must be different");
    }
    let (events_tx, _) = broadcast::channel(256);
    let state = App {
        inner: Arc::new(Mutex::new(Core {
            store: Store::open(&args.db, &args.device_id)?,
            connection: None,
            generation: 0,
        })),
        device_id: args.device_id.into(),
        bridge_token: args.bridge_token.into(),
        client_token: args.client_token.into(),
        events: events_tx,
        streams: Arc::new(Semaphore::new(64)),
        bridge_slots: Arc::new(Semaphore::new(1)),
    };
    let clients = Router::new()
        .route("/v1/devices", get(devices))
        .route("/v1/devices/{device}/commands", post(submit))
        .route("/v1/devices/{device}/commands/{id}", get(command))
        .route("/v1/devices/{device}/events", get(events))
        .route("/v1/devices/{device}/stream", get(event_stream))
        .route_layer(middleware::from_fn_with_state(state.clone(), client_auth));
    let app = Router::new()
        .route("/healthz", get(|| async { Json(json!({"status":"ok"})) }))
        .route("/v1/bridge", get(bridge_upgrade))
        .merge(clients)
        .layer(DefaultBodyLimit::max(131_072))
        .with_state(state);
    if !args.listen.ip().is_loopback() {
        tracing::warn!(
            "listener uses plaintext HTTP; remote access requires a TLS reverse proxy or SSH tunnel"
        );
    }
    let listener = tokio::net::TcpListener::bind(args.listen).await?;
    tracing::info!(address = %listener.local_addr()?, "server listening");
    axum::serve(listener, app)
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await?;
    Ok(())
}

fn authorized(headers: &HeaderMap, token: &str) -> bool {
    let Some(value) = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
    else {
        return false;
    };
    if value.len() != token.len() {
        return false;
    }
    value
        .bytes()
        .zip(token.bytes())
        .fold(0u8, |diff, (a, b)| diff | (a ^ b))
        == 0
}

async fn client_auth(
    State(app): State<App>,
    request: axum::extract::Request,
    next: Next,
) -> Response {
    if !authorized(request.headers(), &app.client_token) {
        return ApiError(
            StatusCode::UNAUTHORIZED,
            "valid client bearer token required".into(),
        )
        .into_response();
    }
    next.run(request).await
}

async fn devices(State(app): State<App>) -> ApiResult<Json<Vec<DeviceStatus>>> {
    let core = app.core()?;
    Ok(Json(vec![DeviceStatus {
        device_id: app.device_id.to_string(),
        online: core.online(),
        last_seen_ms: core.store.last_seen()?,
        last_event_seq: core.store.last_seq()?,
    }]))
}

async fn submit(
    State(app): State<App>,
    Path(device): Path<String>,
    Json(request): Json<CommandRequest>,
) -> ApiResult<(StatusCode, Json<CommandRecord>)> {
    app.device(&device)?;
    request.validate().map_err(ApiError::bad)?;
    let core = app.core()?;
    if let Some(record) = core.store.command(&request.id)? {
        if record.request != request {
            return Err(ApiError(
                StatusCode::CONFLICT,
                "command id already exists with different content".into(),
            ));
        }
        return Ok((StatusCode::ACCEPTED, Json(record)));
    }
    if !core.online() {
        return Err(ApiError(
            StatusCode::SERVICE_UNAVAILABLE,
            "device is offline".into(),
        ));
    }
    if core.store.pending_ids()?.len() >= 256 {
        return Err(ApiError(
            StatusCode::SERVICE_UNAVAILABLE,
            "too many unresolved commands; reconcile existing commands first".into(),
        ));
    }
    let sender = core
        .connection
        .as_ref()
        .expect("online connection")
        .sender
        .clone();
    let permit = sender.try_reserve_owned().map_err(|_| {
        ApiError(
            StatusCode::SERVICE_UNAVAILABLE,
            "bridge command queue is full or closed".into(),
        )
    })?;
    let record = core.store.insert(request.clone())?;
    permit.send(ToBridge::Command { request });
    Ok((StatusCode::ACCEPTED, Json(record)))
}

async fn command(
    State(app): State<App>,
    Path((device, id)): Path<(String, String)>,
) -> ApiResult<Json<CommandRecord>> {
    app.device(&device)?;
    validate_id(&id).map_err(ApiError::bad)?;
    app.core()?
        .store
        .command(&id)?
        .map(Json)
        .ok_or_else(|| ApiError(StatusCode::NOT_FOUND, "command not found".into()))
}

#[derive(Default, Deserialize)]
struct EventQuery {
    #[serde(default)]
    after: u64,
    limit: Option<usize>,
}

fn event_cursor(query: &EventQuery) -> ApiResult<usize> {
    let limit = query.limit.unwrap_or(100);
    if query.after > i64::MAX as u64 || limit == 0 || limit > 500 {
        return Err(ApiError::bad(
            "after must fit a signed 64-bit integer and limit must be 1-500",
        ));
    }
    Ok(limit)
}

async fn events(
    State(app): State<App>,
    Path(device): Path<String>,
    Query(query): Query<EventQuery>,
) -> ApiResult<Json<Vec<vibeface_protocol::BridgeEvent>>> {
    app.device(&device)?;
    let limit = event_cursor(&query)?;
    Ok(Json(app.core()?.store.events(query.after, limit)?))
}

struct EventStream {
    app: App,
    after: u64,
    receiver: broadcast::Receiver<u64>,
    done: bool,
    _permit: OwnedSemaphorePermit,
}

async fn event_stream(
    State(app): State<App>,
    Path(device): Path<String>,
    Query(mut query): Query<EventQuery>,
    headers: HeaderMap,
) -> ApiResult<Response> {
    app.device(&device)?;
    if let Some(last_id) = headers.get("last-event-id") {
        query.after = last_id
            .to_str()
            .map_err(ApiError::bad)?
            .parse()
            .map_err(ApiError::bad)?;
    }
    event_cursor(&query)?;
    let permit = app.streams.clone().try_acquire_owned().map_err(|_| {
        ApiError(
            StatusCode::SERVICE_UNAVAILABLE,
            "too many event streams".into(),
        )
    })?;
    let receiver = app.events.subscribe();
    let stream_state = EventStream {
        app,
        after: query.after,
        receiver,
        done: false,
        _permit: permit,
    };
    let output = stream::unfold(stream_state, |mut state| async move {
        if state.done {
            return None;
        }
        loop {
            if let Err(broadcast::error::TryRecvError::Lagged(_)) = state.receiver.try_recv() {
                state.done = true;
                let event = Event::default().event("reconnect").data(json!({"after":state.after,"reason":"slow consumer; reconnect using this cursor"}).to_string());
                return Some((Ok::<_, Infallible>(event), state));
            }
            let next = match state
                .app
                .core()
                .and_then(|core| core.store.events(state.after, 1).map_err(ApiError::from))
            {
                Ok(mut events) => events.pop(),
                Err(_) => {
                    state.done = true;
                    return Some((
                        Ok(Event::default().event("reconnect").data(
                            json!({"after":state.after,"reason":"storage unavailable"}).to_string(),
                        )),
                        state,
                    ));
                }
            };
            if let Some(event) = next {
                state.after = event.seq;
                return Some((
                    Ok(Event::default()
                        .id(event.seq.to_string())
                        .event("bridge")
                        .data(serde_json::to_string(&event).expect("serializable event"))),
                    state,
                ));
            }
            match state.receiver.recv().await {
                Ok(_) => {}
                Err(_) => {
                    state.done = true;
                    return Some((
                        Ok(Event::default().event("reconnect").data(
                            json!({"after":state.after,"reason":"event stream interrupted"})
                                .to_string(),
                        )),
                        state,
                    ));
                }
            }
        }
    });
    Ok(Sse::new(output)
        .keep_alive(KeepAlive::new().interval(Duration::from_secs(15)))
        .into_response())
}

async fn bridge_upgrade(
    State(app): State<App>,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> ApiResult<Response> {
    if !authorized(&headers, &app.bridge_token) {
        return Err(ApiError(
            StatusCode::UNAUTHORIZED,
            "valid bridge bearer token required".into(),
        ));
    }
    let permit = app.bridge_slots.clone().try_acquire_owned().map_err(|_| {
        ApiError(
            StatusCode::CONFLICT,
            "another bridge is already connected or handshaking".into(),
        )
    })?;
    Ok(ws
        .max_message_size(MAX_MESSAGE)
        .max_frame_size(MAX_MESSAGE)
        .on_upgrade(move |socket| async move {
            let _permit = permit;
            bridge_socket(app, socket).await;
        }))
}

async fn send(socket: &mut WebSocket, message: &ToBridge) -> Result<()> {
    tokio::time::timeout(
        Duration::from_secs(10),
        socket.send(Message::Text(serde_json::to_string(message)?.into())),
    )
    .await??;
    Ok(())
}

async fn bridge_socket(app: App, mut socket: WebSocket) {
    let connected = async {
        let hello = tokio::time::timeout(Duration::from_secs(10), socket.recv())
            .await?
            .context("bridge disconnected before hello")??;
        let Message::Text(text) = hello else {
            bail!("bridge must send a text hello");
        };
        let ToServer::Hello { version, device_id } = serde_json::from_str(&text)? else {
            bail!("bridge must send hello first");
        };
        if version != VERSION || device_id != app.device_id.as_ref() {
            bail!("bridge version or device mismatch");
        }
        let (sender, receiver) = mpsc::channel(64);
        let (generation, welcome) = {
            let mut core = app
                .core()
                .map_err(|_| anyhow::anyhow!("state unavailable"))?;
            if core.connection.is_some() {
                bail!("another bridge is already connected");
            }
            core.store.seen()?;
            let welcome = ToBridge::Welcome {
                version: VERSION,
                last_event_seq: core.store.last_seq()?,
                pending_command_ids: core.store.pending_ids()?,
            };
            core.generation += 1;
            let generation = core.generation;
            core.connection = Some(Connection {
                generation,
                sender,
                heartbeat: Instant::now(),
            });
            (generation, welcome)
        };
        Ok::<_, anyhow::Error>((generation, welcome, receiver))
    }
    .await;
    let (generation, welcome, mut receiver) = match connected {
        Ok(value) => value,
        Err(_) => {
            let _ = socket.close().await;
            return;
        }
    };
    tracing::info!(device = %app.device_id, "bridge connected");
    let session = async {
        send(&mut socket, &welcome).await?;
        let mut watchdog = tokio::time::interval(Duration::from_secs(5));
        loop {
            tokio::select! {
                outbound = receiver.recv() => {
                    match outbound {
                        Some(message) => send(&mut socket, &message).await?,
                        None => break,
                    }
                }
                _ = watchdog.tick() => {
                    let core = app.core().map_err(|_| anyhow::anyhow!("state unavailable"))?;
                    if !core.online() { bail!("bridge heartbeat timed out"); }
                }
                incoming = socket.recv() => {
                    let Some(message) = incoming else { break; };
                    match message? {
                        Message::Text(text) => {
                            let message: ToServer = serde_json::from_str(&text)?;
                            let ack = {
                                let mut core = app.core().map_err(|_| anyhow::anyhow!("state unavailable"))?;
                                match message {
                                    ToServer::Hello { .. } => bail!("hello already received"),
                                    ToServer::Heartbeat => {
                                        core.store.seen()?;
                                        if let Some(connection) = core.connection.as_mut() { connection.heartbeat = Instant::now(); }
                                        None
                                    }
                                    ToServer::Event { event } => {
                                        if event.kind.is_empty() || event.kind.len() > 256 { bail!("invalid event kind"); }
                                        let inserted = core.store.event(&event)?;
                                        if inserted { let _ = app.events.send(event.seq); }
                                        Some(ToBridge::Ack { seq: event.seq })
                                    }
                                    ToServer::CommandUpdate { record } => {
                                        record.request.validate().map_err(anyhow::Error::msg)?;
                                        core.store.update(&record)?;
                                        None
                                    }
                                    ToServer::CommandUnknown { id, error } => {
                                        validate_id(&id).map_err(anyhow::Error::msg)?;
                                        if error.len() > vibeface_protocol::MAX_TEXT_BYTES { bail!("command error is too large"); }
                                        core.store.command_unknown(&id, error)?;
                                        None
                                    }
                                }
                            };
                            if let Some(ack) = ack { send(&mut socket, &ack).await?; }
                        }
                        Message::Ping(_) | Message::Pong(_) => {}
                        Message::Close(_) => break,
                        Message::Binary(_) => bail!("binary bridge messages are unsupported"),
                    }
                }
            }
        }
        Ok::<_, anyhow::Error>(())
    }.await;
    if session.is_err() {
        tracing::warn!(device = %app.device_id, "bridge connection ended with a protocol, transport or storage error");
    }
    if let Ok(mut core) = app.core()
        && core
            .connection
            .as_ref()
            .is_some_and(|c| c.generation == generation)
    {
        core.connection = None;
        if let Err(error) = core.store.mark_unknown() {
            tracing::error!(%error, "could not mark disconnected commands unknown");
        }
    }
    let _ = socket.close().await;
    tracing::info!(device = %app.device_id, "bridge disconnected");
}

async fn client(args: Client) -> Result<()> {
    validate_id(&args.device_id).map_err(anyhow::Error::msg)?;
    let mut base = reqwest::Url::parse(&args.url)?;
    validate_client_url(&base)?;
    base.set_path(&format!("{}/", base.path().trim_end_matches('/')));
    let http = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(10))
        .build()?;
    let device = format!("v1/devices/{}", args.device_id);
    let mut streaming = false;
    let request = match args.action {
        ClientAction::Devices => http.get(base.join("v1/devices")?),
        ClientAction::Command { id } => {
            validate_id(&id).map_err(anyhow::Error::msg)?;
            http.get(base.join(&format!("{device}/commands/{id}"))?)
        }
        ClientAction::Submit { file } => {
            let bytes = if file.as_os_str() == "-" {
                use std::io::Read;
                let mut bytes = Vec::new();
                io::stdin().take(131_073).read_to_end(&mut bytes)?;
                bytes
            } else {
                tokio::fs::read(file).await?
            };
            if bytes.len() > 131_072 {
                bail!("command file exceeds 131072 bytes");
            }
            let command: CommandRequest = serde_json::from_slice(&bytes)?;
            command.validate().map_err(anyhow::Error::msg)?;
            http.post(base.join(&format!("{device}/commands"))?)
                .json(&command)
        }
        ClientAction::Events { after, limit } => http
            .get(base.join(&format!("{device}/events"))?)
            .query(&[("after", after.to_string()), ("limit", limit.to_string())]),
        ClientAction::Stream { after } => {
            streaming = true;
            http.get(base.join(&format!("{device}/stream"))?)
                .query(&[("after", after.to_string())])
        }
    };
    let request = if streaming {
        request
    } else {
        request.timeout(Duration::from_secs(30))
    };
    let response = request.bearer_auth(args.token).send().await?;
    if !response.status().is_success() {
        let status = response.status();
        bail!("server returned {status}: {}", response.text().await?);
    }
    if streaming {
        let mut chunks = response.bytes_stream();
        while let Some(chunk) = chunks.next().await {
            io::stdout().write_all(&chunk?)?;
            io::stdout().flush()?;
        }
    } else {
        println!(
            "{}",
            serde_json::to_string_pretty(&response.json::<serde_json::Value>().await?)?
        );
    }
    Ok(())
}

fn validate_client_url(url: &reqwest::Url) -> Result<()> {
    let loopback = url.host_str().is_some_and(|host| {
        host == "localhost"
            || host
                .trim_matches(['[', ']'])
                .parse::<std::net::IpAddr>()
                .is_ok_and(|ip| ip.is_loopback())
    });
    if !(url.scheme() == "https" || url.scheme() == "http" && loopback)
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        bail!("url must use HTTPS or loopback HTTP without credentials, query or fragment");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn client_token_only_travels_over_tls_or_loopback() {
        for url in [
            "https://relay.example",
            "http://127.0.0.1:8787",
            "http://[::1]:8787",
            "http://localhost:8787",
        ] {
            assert!(validate_client_url(&url.parse().unwrap()).is_ok());
        }
        for url in [
            "http://relay.example",
            "https://user:password@relay.example",
            "https://relay.example?token=secret",
        ] {
            assert!(validate_client_url(&url.parse().unwrap()).is_err());
        }
    }

    #[test]
    fn bearer_auth_distinguishes_credentials_and_rejects_missing_headers() {
        let mut headers = HeaderMap::new();
        assert!(!authorized(&headers, "client-secret"));
        headers.insert(
            header::AUTHORIZATION,
            "Bearer client-secret".parse().unwrap(),
        );
        assert!(authorized(&headers, "client-secret"));
        assert!(!authorized(&headers, "bridge-secret"));
        assert!(
            event_cursor(&EventQuery {
                after: u64::MAX,
                limit: None
            })
            .is_err()
        );
        assert!(
            event_cursor(&EventQuery {
                after: 0,
                limit: Some(501)
            })
            .is_err()
        );
    }
}
