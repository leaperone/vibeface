//! Browser and CLI sessions over `/api/client`.
//!
//! Both ends prove their device key inside the Noise handshake. Membership is read again for
//! every request, so a device that joins mid-session gains access and a revoked one loses it.

use crate::{
    crypto::LocalIdentity,
    identity::{
        AuthorizationView, Genesis, GovernanceAction, GovernanceEvent, IdentityKind, NetworkState,
        RootCheckpoint,
        local::{Join, JoinRequest, JoinStatus},
    },
    net::{self, Net, NetworkConfig},
    runtime::{Access, Runtime},
};
use anyhow::{Context, Result, anyhow, bail, ensure};
use axum::extract::ws::{Message, WebSocket};
use futures_util::{SinkExt, StreamExt};
use iroh::{
    EndpointAddr, TransportAddr,
    endpoint::{Connection, ReadExactError, RecvStream, SendStream},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    path::Path,
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message as NativeMessage;
use vibeface_secure::{
    Channel,
    join::{InviteRecord, Ticket, invite_id, join_commit, short_code},
};

pub const PROLOGUE: &[u8] = b"vibeface:client:v1";
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
const CALL_TIMEOUT: Duration = Duration::from_secs(120);
const IDLE_TIMEOUT: Duration = Duration::from_secs(90);
const HEARTBEAT: Duration = Duration::from_secs(20);
const MAX_FORWARDED_FRAME: usize = 1024 * 1024;

type NativeSocket =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

/// A client channel: the browser's WebSocket, the CLI's, or a stream another Gateway forwards
/// over `vibeface/client/1`. Forwarded frames are `tag ‖ u32 length ‖ bytes`.
pub enum ClientSocket {
    Server(Box<WebSocket>),
    Client(Box<NativeSocket>),
    Forwarded(Box<Forwarded>),
}

pub struct Forwarded {
    send: SendStream,
    frames: mpsc::Receiver<Result<Message>>,
    reader: tokio::task::JoinHandle<()>,
}
impl Drop for Forwarded {
    fn drop(&mut self) {
        self.reader.abort();
    }
}

impl ClientSocket {
    pub fn server(socket: WebSocket) -> Self {
        Self::Server(Box::new(socket))
    }

    fn forwarded(send: SendStream, mut recv: RecvStream) -> Self {
        let (frames, receiver) = mpsc::channel(16);
        let reader = tokio::spawn(async move {
            loop {
                let frame = match read_forwarded(&mut recv).await {
                    Ok(Some(frame)) => Ok(frame),
                    Ok(None) => return,
                    Err(error) => Err(error),
                };
                let failed = frame.is_err();
                if frames.send(frame).await.is_err() || failed {
                    return;
                }
            }
        });
        Self::Forwarded(Box::new(Forwarded {
            send,
            frames: receiver,
            reader,
        }))
    }

    /// Cancel-safe, so callers may race it in `select!`.
    pub async fn next(&mut self) -> Option<Result<Message>> {
        match self {
            Self::Server(socket) => socket.next().await.map(|frame| frame.map_err(Into::into)),
            Self::Client(socket) => loop {
                let frame = match socket.next().await? {
                    Ok(frame) => frame,
                    Err(error) => return Some(Err(error.into())),
                };
                return Some(Ok(match frame {
                    NativeMessage::Text(text) => Message::Text(text.as_str().to_owned().into()),
                    NativeMessage::Binary(bytes) => Message::Binary(bytes),
                    NativeMessage::Ping(bytes) => Message::Ping(bytes),
                    NativeMessage::Pong(bytes) => Message::Pong(bytes),
                    NativeMessage::Close(_) => Message::Close(None),
                    NativeMessage::Frame(_) => continue,
                }));
            },
            Self::Forwarded(forwarded) => forwarded.frames.recv().await,
        }
    }

    pub async fn send(&mut self, message: Message) -> Result<()> {
        match self {
            Self::Server(socket) => socket.send(message).await.map_err(Into::into),
            Self::Client(socket) => socket
                .send(match message {
                    Message::Text(text) => NativeMessage::Text(text.as_str().to_owned().into()),
                    Message::Binary(bytes) => NativeMessage::Binary(bytes),
                    Message::Ping(bytes) => NativeMessage::Ping(bytes),
                    Message::Pong(bytes) => NativeMessage::Pong(bytes),
                    Message::Close(_) => NativeMessage::Close(None),
                })
                .await
                .map_err(Into::into),
            Self::Forwarded(forwarded) => {
                let (tag, bytes) = match message {
                    Message::Binary(bytes) => (0, bytes.to_vec()),
                    Message::Text(text) => (1, text.as_str().as_bytes().to_vec()),
                    Message::Ping(bytes) => (2, bytes.to_vec()),
                    Message::Pong(bytes) => (3, bytes.to_vec()),
                    Message::Close(_) => {
                        forwarded.send.finish().ok();
                        return Ok(());
                    }
                };
                ensure!(bytes.len() <= MAX_FORWARDED_FRAME, "client frame too large");
                let mut frame = vec![tag];
                frame.extend_from_slice(&(bytes.len() as u32).to_be_bytes());
                frame.extend_from_slice(&bytes);
                forwarded
                    .send
                    .write_all(&frame)
                    .await
                    .map_err(|error| anyhow!("forwarded client stream failed: {error}"))
            }
        }
    }
}

async fn read_forwarded(recv: &mut RecvStream) -> Result<Option<Message>> {
    let mut header = [0u8; 5];
    match recv.read_exact(&mut header).await {
        Ok(()) => {}
        Err(ReadExactError::FinishedEarly(0)) => return Ok(None),
        Err(error) => bail!("forwarded client stream failed: {error}"),
    }
    let length = u32::from_be_bytes([header[1], header[2], header[3], header[4]]) as usize;
    ensure!(length <= MAX_FORWARDED_FRAME, "client frame too large");
    let mut bytes = vec![0; length];
    recv.read_exact(&mut bytes)
        .await
        .map_err(|error| anyhow!("forwarded client stream failed: {error}"))?;
    Ok(Some(match header[0] {
        0 => Message::Binary(bytes.into()),
        1 => Message::Text(String::from_utf8(bytes)?.into()),
        2 => Message::Ping(bytes.into()),
        3 => Message::Pong(bytes.into()),
        _ => bail!("unknown forwarded client frame"),
    }))
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Request {
    id: String,
    method: String,
    #[serde(default)]
    params: Value,
}
#[derive(Serialize)]
struct Response {
    id: String,
    result: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

/// First text frame on `/api/client`.
#[derive(Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
enum Connect {
    Connect { target: String },
}

struct Session {
    device_id: String,
    /// Present only on `/api/local/ws`; it lets a non-member browser use this Node.
    cookie: Option<String>,
    subscribed: bool,
    observation: Option<crate::runtime::ObservationGuard>,
}

#[derive(Serialize)]
struct Principal {
    source: &'static str,
    network_id: String,
    device_id: String,
    epoch: u64,
    can_manage: bool,
}

pub struct Client {
    socket: ClientSocket,
    channel: Channel,
}
impl Client {
    /// Connects through the Gateway at `origin` and pins `target`'s device key.
    pub async fn connect(origin: &str, identity: &LocalIdentity, target: &str) -> Result<Self> {
        let (socket, _) = tokio::time::timeout(
            HANDSHAKE_TIMEOUT,
            tokio_tungstenite::connect_async(client_url(origin)?),
        )
        .await
        .context("Gateway connection timed out")??;
        let mut socket = ClientSocket::Client(Box::new(socket));
        let connect = serde_json::to_string(&Connect::Connect {
            target: target.to_owned(),
        })?;
        socket.send(Message::Text(connect.into())).await?;
        Self::from_socket(socket, identity, target).await
    }
    async fn from_socket(
        mut socket: ClientSocket,
        identity: &LocalIdentity,
        target: &str,
    ) -> Result<Self> {
        let channel = tokio::time::timeout(HANDSHAKE_TIMEOUT, async {
            let mut handshake =
                secure(identity.handshake(true, Some(target.to_owned()), PROLOGUE))?;
            send_frame(&mut socket, secure(handshake.write(&[]))?).await?;
            let second = next_binary(&mut socket).await?;
            ensure!(
                secure(handshake.read(&second))?.is_empty(),
                "unexpected client handshake payload"
            );
            send_frame(&mut socket, secure(handshake.write(&[]))?).await?;
            secure(handshake.finish())
        })
        .await
        .context("client handshake timed out")??;
        Ok(Self { socket, channel })
    }
    pub async fn call(&mut self, method: &str, params: Value) -> Result<Value> {
        let id = uuid::Uuid::new_v4().to_string();
        let request = Request {
            id: id.clone(),
            method: method.to_owned(),
            params,
        };
        send_value(&mut self.socket, &mut self.channel, &request).await?;
        tokio::time::timeout(CALL_TIMEOUT, async {
            loop {
                let value: Value = serde_json::from_slice(
                    &receive_value(&mut self.socket, &mut self.channel).await?,
                )?;
                if value["id"].as_str() != Some(&id) {
                    continue;
                }
                if let Some(error) = value["error"].as_str() {
                    bail!("{error}");
                }
                return Ok(value["result"].clone());
            }
        })
        .await
        .context("client response timed out; outcome may be unknown")?
    }
}

/// `/api/client`: read the connect frame, then end Noise here when the target is this Node,
/// or relay the ciphertext to the target member Node over `vibeface/client/1`.
pub async fn serve_gateway(node: Arc<Runtime>, mut socket: ClientSocket) -> Result<()> {
    let frame = tokio::time::timeout(HANDSHAKE_TIMEOUT, socket.next())
        .await
        .context("connect frame timed out")?
        .context("client disconnected")??;
    let Message::Text(text) = frame else {
        bail!("the first client frame must be a connect request")
    };
    let Connect::Connect { target } =
        serde_json::from_str(&text).context("invalid connect frame")?;
    if target == node.identity.device_id()? && !node.config.public_entry {
        return serve_session(node, socket, None).await;
    }
    let forwarded = match open_forward(&node, &target).await {
        Ok(forwarded) => forwarded,
        Err((code, error)) => {
            let refusal = json!({"type":"error","code":code,"error":format!("{error:#}")});
            let _ = socket.send(Message::Text(refusal.to_string().into())).await;
            return Err(error.context("client target refused"));
        }
    };
    relay_frames(socket, forwarded).await
}

async fn open_forward(
    node: &Runtime,
    target: &str,
) -> std::result::Result<ClientSocket, (&'static str, anyhow::Error)> {
    if node.config.public_entry {
        // It holds no address book: the target must be on this entry's relay, which only
        // token holders reach.
        let reach = async {
            let relay = node
                .config
                .network
                .own_relay()?
                .context("a public entry runs a relay")?;
            let addr =
                EndpointAddr::from_parts(net::endpoint_id(target)?, [TransportAddr::Relay(relay)]);
            node.net()?.learn(&addr).await;
            Ok::<_, anyhow::Error>(())
        };
        reach.await.map_err(|error| ("target_unreachable", error))?;
    } else {
        let member = async {
            let network = node.membership_state().await?.genesis.network_id;
            node.store.lock().await.authorize_node(&network, target)
        };
        member.await.map_err(|error| ("target_not_member", error))?;
    }
    let opened = async {
        let connection = node.net()?.connect(target, net::CLIENT).await?;
        let (send, recv) = connection
            .open_bi()
            .await
            .map_err(|error| anyhow!("target connection closed: {error}"))?;
        Ok::<_, anyhow::Error>(ClientSocket::forwarded(send, recv))
    };
    opened.await.map_err(|error| ("target_unreachable", error))
}

/// Copies frames both ways without reading them; the Noise session ends at the target.
async fn relay_frames(mut client: ClientSocket, mut target: ClientSocket) -> Result<()> {
    loop {
        tokio::select! {
            frame = client.next() => match frame {
                Some(Ok(Message::Close(_))) | None => return Ok(()),
                Some(frame) => target.send(frame?).await?,
            },
            frame = target.next() => match frame {
                None => return Ok(()),
                Some(frame) => client.send(frame?).await?,
            },
        }
    }
}

/// The target side of `vibeface/client/1`: each stream is one forwarded client session.
pub async fn serve_forwarded(node: Arc<Runtime>, connection: Connection) -> Result<()> {
    while let Ok((send, recv)) = connection.accept_bi().await {
        let node = node.clone();
        tokio::spawn(async move {
            if let Err(error) = serve_session(node, ClientSocket::forwarded(send, recv), None).await
            {
                tracing::debug!(%error, "forwarded client session closed");
            }
        });
    }
    Ok(())
}

/// `vibeface/join/1`: any device may call `join` and `join_status`, nothing else. QUIC proves
/// the caller's key, so the session's device is the connection's remote ID.
pub async fn serve_join(node: Arc<Runtime>, connection: Connection, remote: String) -> Result<()> {
    net::serve_streams(connection, move |request: Value| {
        let (node, remote) = (node.clone(), remote.clone());
        async move {
            let method = text(&request, "method")?;
            ensure!(
                matches!(method, "join" | "join_status") && request["params"].is_object(),
                "a join connection only joins"
            );
            let mut session = Session {
                device_id: remote,
                cookie: None,
                subscribed: false,
                observation: None,
            };
            let result = dispatch(&node, &mut session, method, request["params"].clone()).await?;
            Ok(json!({"result":result}))
        }
    })
    .await;
    Ok(())
}

pub async fn serve_local(node: Arc<Runtime>, socket: ClientSocket, cookie: String) -> Result<()> {
    serve_session(node, socket, Some(cookie)).await
}

async fn serve_session(
    node: Arc<Runtime>,
    mut socket: ClientSocket,
    cookie: Option<String>,
) -> Result<()> {
    let mut local_changes = node.local_access.subscribe();
    let (mut channel, device_id) = tokio::time::timeout(HANDSHAKE_TIMEOUT, async {
        let mut handshake = secure(node.identity.handshake(false, None, PROLOGUE))?;
        ensure!(
            secure(handshake.read(&next_binary(&mut socket).await?))?.is_empty(),
            "unexpected first client payload"
        );
        send_frame(&mut socket, secure(handshake.write(&[]))?).await?;
        ensure!(
            secure(handshake.read(&next_binary(&mut socket).await?))?.is_empty(),
            "unexpected final client payload"
        );
        let device_id = handshake.remote_id().context("client identity missing")?;
        Ok::<_, anyhow::Error>((secure(handshake.finish())?, device_id))
    })
    .await
    .context("client handshake timed out")??;
    if let Some(cookie) = &cookie {
        let view = node.membership_state().await?.project()?;
        if view.client(&device_id).is_err() {
            let local =
                node.local_access
                    .session(cookie, &node.identity.device_id()?, &view.network_id)?;
            ensure!(
                local.device_id == device_id,
                "local browser cookie is bound to another device"
            );
        }
    }
    let mut session = Session {
        device_id,
        cookie,
        subscribed: false,
        observation: None,
    };
    let mut events = node.events.subscribe();
    let mut heartbeat = tokio::time::interval(HEARTBEAT);
    heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut received = Instant::now();
    let mut partial_started: Option<Instant> = None;
    loop {
        tokio::select! {
            frame = socket.next() => {
                received = Instant::now();
                match frame.context("client disconnected")?? {
                    Message::Binary(bytes) => {
                        if let Some(plaintext) = secure(channel.open_record(&bytes))? {
                            partial_started = None;
                            let request: Request = serde_json::from_slice(&plaintext).context("invalid client request")?;
                            validate_request(&request)?;
                            let local = is_local_only(&node, &session).await?;
                            let result = if local && request.method != "local_access" {
                                tokio::select! {
                                    result = dispatch(&node, &mut session, &request.method, request.params) => result,
                                    _ = local_changes.changed() => bail!("local browser access was revoked"),
                                }
                            } else {
                                dispatch(&node, &mut session, &request.method, request.params).await
                            };
                            let response = match result { Ok(result) => Response { id: request.id, result, error: None }, Err(error) => Response { id: request.id, result: Value::Null, error: Some(error.to_string()) } };
                            send_value(&mut socket, &mut channel, &response).await?;
                        } else { partial_started.get_or_insert_with(Instant::now); }
                    }
                    Message::Ping(_) | Message::Pong(_) => {},
                    _ => bail!("client channel accepts only encrypted binary frames"),
                }
            }
            event = events.recv(), if session.subscribed => {
                let principal = principal(&node, &session).await?;
                match event {
                    Ok(event) if event.network_id == principal.network_id => send_value(&mut socket, &mut channel, &json!({"type":"changed","payload":event.payload})).await?,
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => send_value(&mut socket, &mut channel, &json!({"type":"changed"})).await?,
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => return Ok(()),
                    _ => {},
                }
            }
            _ = heartbeat.tick() => {
                ensure!(received.elapsed() < IDLE_TIMEOUT, "client channel idle");
                ensure!(partial_started.is_none_or(|at| at.elapsed() < IDLE_TIMEOUT), "client fragmented message timed out");
                if session.subscribed || session.observation.is_some() { principal(&node, &session).await?; }
                if let Some(observation) = &session.observation { observation.refresh(); }
                tokio::time::timeout(HANDSHAKE_TIMEOUT, socket.send(Message::Ping(Vec::new().into()))).await.context("client heartbeat send timed out")??;
            }
            changed = local_changes.changed(), if session.cookie.is_some() => {
                changed.context("local access state closed")?;
                if is_local_only(&node, &session).await? {
                    bail!("local browser access was revoked");
                }
            }
        }
    }
}

/// Methods a device that is not (or no longer) a member may call.
const GUEST_METHODS: [&str; 6] = [
    "info",
    "authenticate",
    "join",
    "join_status",
    "recovery_state",
    "recover",
];

/// Domain of the recovery root's short-lived permission to read the membership state.
const RECOVERY_READ_DOMAIN: &str = "identity-recovery-read";

async fn dispatch(
    node: &Arc<Runtime>,
    session: &mut Session,
    method: &str,
    params: Value,
) -> Result<Value> {
    let _work = if matches!(
        method,
        "info"
            | "authenticate"
            | "directory"
            | "node_settings"
            | "subscribe"
            | "join_status"
            | "recovery_state"
            | "entrances"
    ) {
        None
    } else {
        Some(node.maintenance.enter()?)
    };
    if !GUEST_METHODS.contains(&method) {
        principal(node, session).await?;
    }
    match method {
        "info" => {
            let network_id = match node.has_membership().await? {
                true => Some(node.membership_state().await?.genesis.network_id),
                false => None,
            };
            Ok(
                json!({"initialized":network_id.is_some(),"network_id":network_id,"device_id":node.identity.device_id()?,"name":crate::config::hostname(),"roles":node.config.roles,"version":env!("CARGO_PKG_VERSION"),"connection_entry":connection_entry(node).await?}),
            )
        }
        "authenticate" => {
            let authenticated = authentication(node, session).await?;
            if let Some(network) = params
                .get("network_id")
                .filter(|value| value.as_str().is_some_and(|text| !text.is_empty()))
                && authenticated["network_id"] != *network
            {
                bail!("client requested another personal network");
            }
            Ok(authenticated)
        }
        "directory" => {
            let state = node.membership_state().await?;
            Ok(json!({"view":state.project()?,"state":state}))
        }
        "write_state" => {
            let principal = principal(node, session).await?;
            node.write_status(&principal.network_id).await
        }
        "confirm_writable" => {
            node_operator(node, session).await?;
            let principal = principal(node, session).await?;
            node.confirm_writable(&principal.network_id, &session.device_id)
                .await?;
            node.write_status(&principal.network_id).await
        }
        "node_settings" => {
            node_operator(node, session).await?;
            node.node_settings().await
        }
        "configure_node" => {
            node_operator(node, session).await?;
            let settings = serde_json::from_value(params).context("invalid Node settings")?;
            let cookie = match is_local_only(node, session).await? {
                true => session.cookie.as_deref(),
                false => None,
            };
            node.configure_node(settings, &session.device_id, cookie)
                .await
        }
        "subscribe" => {
            // The browser sends the vector it has already shown; a lagging entry asks its peers now.
            let principal = principal(node, session).await?;
            session.subscribed = true;
            let vector: std::collections::BTreeMap<String, u64> = node
                .store
                .lock()
                .await
                .heads(&principal.network_id)?
                .into_iter()
                .map(|(author, (seq, _))| (author, seq))
                .collect();
            let behind = params["vector"].as_object().is_some_and(|seen| {
                seen.iter().any(|(author, seq)| {
                    seq.as_u64()
                        .is_some_and(|seq| vector.get(author).is_none_or(|have| *have < seq))
                })
            });
            if behind {
                node.wake_sync_all();
            }
            Ok(json!({"subscribed":true,"vector":vector,"behind":behind}))
        }
        "rpc" => {
            let principal = principal(node, session).await?;
            let mut params = params;
            bind_client_identity(&mut params, &principal.device_id)?;
            let watched_scope = (params["method"] == "conversation")
                .then(|| params["conversation_id"].as_str().map(String::from))
                .flatten();
            let access = Access {
                network_id: principal.network_id,
                human: true,
                conversation_id: None,
                manager: None,
            };
            let mut result = node.rpc(access.clone(), params).await?;
            if let Some(scope) = watched_scope {
                session.observation.take();
                let targets = result["observation_targets"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|value| value.as_str().map(String::from))
                    .collect();
                match node.observe_conversation(&access, &scope, targets).await {
                    Ok(observation) => session.observation = Some(observation),
                    Err(error) => result["observation_error"] = json!(error.to_string()),
                }
            }
            Ok(result)
        }
        "local_access" => {
            let _save = node.config_save.lock().await;
            let principal = principal(node, session).await?;
            if let Some(enabled) = params.get("enabled") {
                let enabled = enabled.as_bool().context("enabled must be a boolean")?;
                ensure!(
                    principal.can_manage || (principal.source == "local_session" && !enabled),
                    "only a management client can enable local browser access"
                );
                node.local_access.set_enabled(enabled)?;
            }
            Ok(json!({"enabled":node.local_access.enabled()}))
        }
        "invite" => {
            manager_view(node, session).await?;
            let record: InviteRecord = from_field(&params, "record")?;
            let join_id = node
                .store
                .lock()
                .await
                .register_invite(&record, &session.device_id)?;
            // Only this Node's own relay knows the invite, so only it admits the joiner's token.
            let own_relay = node.config.network.own_relay().ok().flatten();
            let entry = node.net().ok().map(|net| {
                let addr = net.addr();
                json!({"relay_url":own_relay.or_else(|| addr.relay_urls().next().cloned()),
                    "direct_addrs":addr.ip_addrs().take(8).collect::<Vec<_>>()})
            });
            Ok(json!({"join_id":join_id,"entry_id":node.identity.device_id()?,"entry":entry}))
        }
        "entrances" => {
            let principal = principal(node, session).await?;
            Ok(json!({"entrances":node.entrances(&principal.network_id).await?}))
        }
        "join" => {
            let join: Join = serde_json::from_value(params).context("invalid join request")?;
            ensure!(
                join.public_key == session.device_id,
                "join key differs from this channel's device"
            );
            let request = node.store.lock().await.join(&join, &session.device_id)?;
            notify_join(node, &request);
            Ok(serde_json::to_value(request)?)
        }
        "join_status" => {
            let Some(join_id) = params.get("join_id") else {
                manager_view(node, session).await?;
                let mut pending = serde_json::to_value(
                    node.store.lock().await.pending_joins(&session.device_id)?,
                )?;
                // A join waits on the Node whose invite it used; managers see it from any entry.
                if let Some(local) = pending.as_array_mut() {
                    for remote in node.remote_joins(&session.device_id).await {
                        if !local
                            .iter()
                            .any(|known| known["join_id"] == remote["join_id"])
                        {
                            local.push(remote);
                        }
                    }
                }
                return Ok(json!({"requests":pending}));
            };
            let join_id = join_id.as_str().context("join_id must be a string")?;
            let reveal = params.get("reveal").and_then(Value::as_str);
            let request =
                node.store
                    .lock()
                    .await
                    .join_status(join_id, &session.device_id, reveal)?;
            if reveal.is_some() {
                notify_join(node, &request);
            }
            let mut result = serde_json::to_value(&request)?;
            if request.status == JoinStatus::Joined {
                let state = node.membership_state().await?;
                result["addresses"] = json!(
                    node.store
                        .lock()
                        .await
                        .address_book(&state.genesis.network_id)?
                );
                result["state"] = serde_json::to_value(state)?;
            }
            Ok(result)
        }
        "approve" => {
            manager_view(node, session).await?;
            let join_id = text(&params, "join_id")?;
            if node.store.lock().await.holds_join(join_id)? {
                approve_join(node, &session.device_id, &params).await
            } else {
                node.forward_join_approval(&session.device_id, &params)
                    .await
            }
        }
        "revoke" => {
            manager_view(node, session).await?;
            let event: GovernanceEvent = from_field(&params, "event")?;
            ensure!(
                event.issuer == session.device_id
                    && matches!(event.action, GovernanceAction::Revoke { .. }),
                "invalid revoke signer or action"
            );
            let mut state = node.membership_state().await?;
            state.append(event)?;
            node.install_membership(state.clone()).await?;
            authentication(node, session).await
        }
        "recovery_state" => {
            // The recovery root may not be a member any more, so it proves itself
            // with a two-minute signature bound to this channel's device.
            let state = node.membership_state().await?;
            let expires = params["expires_at_ms"]
                .as_i64()
                .context("expires_at_ms must be an integer")?;
            let now = crate::runtime::now_ms();
            ensure!(
                expires > now && expires <= now + 120_000,
                "recovery proof expired"
            );
            let payload = format!(
                r#"{{"device_id":"{}","expires_at_ms":{expires},"network_id":"{}"}}"#,
                session.device_id, state.genesis.network_id
            );
            vibeface_secure::verify(
                &state.genesis.network_id,
                RECOVERY_READ_DOMAIN,
                payload.as_bytes(),
                text(&params, "signature")?,
            )
            .map_err(anyhow::Error::msg)
            .context("recovery key does not own this personal network")?;
            Ok(json!({"state":state}))
        }
        "recover" => {
            recover(node, session, &params).await?;
            authentication(node, session).await
        }
        _ => bail!("unknown client method"),
    }
}

/// Creates the network on an empty Node, or installs a higher root checkpoint.
///
/// A root signature authorizes this; a fresh Node also needs its one-time capability.
async fn recover(node: &Runtime, session: &Session, params: &Value) -> Result<()> {
    let genesis: Option<Genesis> = params
        .get("genesis")
        .map(|_| from_field(params, "genesis"))
        .transpose()?;
    let checkpoint: Option<RootCheckpoint> = params
        .get("checkpoint")
        .map(|_| from_field(params, "checkpoint"))
        .transpose()?;
    if !node.has_membership().await? {
        let mut state = NetworkState::new(genesis.context("genesis is required")?)?;
        if let Some(checkpoint) = checkpoint {
            state.recover(checkpoint)?;
        }
        state.project()?.node(&node.identity.device_id()?)?;
        node.store.lock().await.bootstrap_network(
            state.clone(),
            text(params, "capability")?,
            &session.device_id,
        )?;
        node.install_membership(state).await?;
        return Ok(());
    }
    let mut state = node.membership_state().await?;
    ensure!(
        genesis.is_none_or(|genesis| genesis == state.genesis),
        "personal network already initialized"
    );
    let Some(checkpoint) = checkpoint else {
        return Ok(());
    };
    if state.recover(checkpoint)? {
        state
            .project()?
            .node(&node.identity.device_id()?)
            .context("this recovery removes this Node; send it to a Node that stays")?;
        node.install_membership(state.clone()).await?;
    }
    Ok(())
}

/// One approval step for a join this Node holds; `admin` is the authenticated manager, here or
/// at the entry that forwarded it.
pub(crate) async fn approve_join(node: &Runtime, admin: &str, params: &Value) -> Result<Value> {
    let join_id = text(params, "join_id")?;
    let request = if params["reject"] == true {
        node.store.lock().await.reject_join(join_id, admin)?
    } else if let Some(nonce) = params.get("admin_nonce") {
        let nonce = nonce.as_str().context("admin_nonce must be a string")?;
        node.store
            .lock()
            .await
            .approve_nonce(join_id, admin, nonce)?
    } else {
        let event: GovernanceEvent = from_field(params, "event")?;
        let (request, state) = node
            .store
            .lock()
            .await
            .approve_grant(join_id, admin, event)?;
        node.install_membership(state.clone()).await?;
        request
    };
    notify_join(node, &request);
    Ok(serde_json::to_value(request)?)
}

fn notify_join(node: &Runtime, request: &JoinRequest) {
    let _ = node.events.send(crate::runtime::Update {
        network_id: request.network_id.clone(),
        payload: json!({"type":"join","join_id":request.join_id,"status":request.status}),
    });
}

/// Reads membership; a member wins over a local cookie.
async fn principal(node: &Runtime, session: &Session) -> Result<Principal> {
    let view = node.membership_state().await?.project()?;
    if let Ok(member) = view.client(&session.device_id) {
        return Ok(Principal {
            source: "membership",
            network_id: view.network_id,
            device_id: member.device_id,
            epoch: view.epoch,
            can_manage: member.manage,
        });
    }
    let cookie = session
        .cookie
        .as_deref()
        .context("this device is not a member of the personal network")?;
    let node_id = node.identity.device_id()?;
    view.node(&node_id)?;
    let local = node
        .local_access
        .session(cookie, &node_id, &view.network_id)?;
    ensure!(
        local.device_id == session.device_id,
        "local browser session belongs to another device"
    );
    Ok(Principal {
        source: "local_session",
        network_id: local.network_id,
        device_id: local.device_id,
        epoch: view.epoch,
        can_manage: false,
    })
}

async fn is_local_only(node: &Runtime, session: &Session) -> Result<bool> {
    Ok(session.cookie.is_some()
        && (!node.has_membership().await?
            || node
                .membership_state()
                .await?
                .project()?
                .client(&session.device_id)
                .is_err()))
}

async fn node_operator(node: &Runtime, session: &Session) -> Result<()> {
    let principal = principal(node, session).await?;
    ensure!(
        principal.can_manage || principal.source == "local_session",
        "management permission or local browser access required"
    );
    Ok(())
}

async fn manager_view(node: &Runtime, session: &Session) -> Result<AuthorizationView> {
    let view = node.membership_state().await?.project()?;
    view.manager(&session.device_id)?;
    Ok(view)
}

/// Members get the directory; anyone else learns only whether this device was revoked.
async fn authentication(node: &Runtime, session: &Session) -> Result<Value> {
    if !node.has_membership().await? {
        return Ok(json!({"principal":null,"initialized":false,"revoked":false}));
    }
    let state = node.membership_state().await?;
    let view = state.project()?;
    match principal(node, session).await {
        Ok(principal) => Ok(
            json!({"principal":principal,"network_id":view.network_id,"state":state,"view":view,"local_auto_access":node.local_access.enabled()}),
        ),
        Err(_) => Ok(
            json!({"principal":null,"initialized":true,"network_id":view.network_id,"revoked":view.cutoff(&session.device_id).is_some()}),
        ),
    }
}

async fn connection_entry(node: &Runtime) -> Result<Value> {
    let url = reqwest::Url::parse(&node.config.public_url)?;
    Ok(json!({"address":url.origin().ascii_serialization(),"device_id":node.identity.device_id()?}))
}

fn bind_client_identity(input: &mut Value, subject: &str) -> Result<()> {
    if let Some(signer) = input.pointer("/attestation/device_id") {
        ensure!(
            signer.as_str() == Some(subject),
            "the write is signed by another device"
        );
    }
    if let Some(client) = input.get("client_id") {
        ensure!(
            client.as_str() == Some(subject),
            "client_id differs from authenticated client"
        );
    }
    if matches!(
        input["method"].as_str(),
        Some("message" | "presence" | "notifications" | "ack")
    ) {
        input
            .as_object_mut()
            .context("RPC input must be an object")?
            .insert("client_id".to_owned(), Value::String(subject.to_owned()));
    }
    Ok(())
}

/// `http(s)://host` → `ws(s)://host/api/client`.
fn client_url(origin: &str) -> Result<String> {
    let mut url = reqwest::Url::parse(origin)?;
    let scheme = match url.scheme() {
        "http" => "ws",
        "https" => "wss",
        _ => bail!("Gateway URL must be HTTP or HTTPS"),
    };
    ensure!(
        url.host_str().is_some() && url.username().is_empty() && url.password().is_none(),
        "Gateway URL must not contain credentials"
    );
    url.set_scheme(scheme)
        .map_err(|_| anyhow::anyhow!("invalid Gateway URL"))?;
    url.set_path("/api/client");
    url.set_query(None);
    url.set_fragment(None);
    Ok(url.into())
}

/// Splits `https://origin/join#vibeface1…` into the Gateway origin and the ticket.
pub fn parse_join_link(link: &str) -> Result<(String, Ticket)> {
    let url = reqwest::Url::parse(link.trim()).context("invite link must be a URL")?;
    ensure!(
        matches!(url.scheme(), "http" | "https") && url.path() == "/join",
        "invite link must look like https://host/join#vibeface1…"
    );
    let ticket = Ticket::decode(url.fragment().context("invite link has no ticket")?)
        .map_err(anyhow::Error::msg)?;
    Ok((url.origin().ascii_serialization(), ticket))
}

/// Saved so an interrupted `vibeface join` resumes with the same commitment.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PendingJoin {
    link: String,
    name: String,
    nonce: String,
}

/// The joining Node's side of `vibeface/join/1`, through the ticket's addresses. The relay
/// admits a non-member holding the invite's derived token.
struct JoinLink {
    net: Net,
    entry: String,
}
impl JoinLink {
    async fn open(node: &Runtime, ticket: &Ticket) -> Result<Self> {
        let entry = ticket.entry_id();
        let token = net::relay_token(&ticket.invite_hash());
        let relays = ticket
            .entry
            .relay_url
            .iter()
            .map(|url| {
                let url = net::relay_url(url)?;
                // A public entry's relay knows no invites; it admits its operator token.
                let token = match (&node.config.network.entry, &node.entry_token) {
                    (Some(entry), Some(own)) if net::relay_url(&entry.relay)? == url => own.clone(),
                    _ => token.clone(),
                };
                Ok((url, Some(token)))
            })
            .collect::<Result<Vec<_>>>()?;
        let mut addrs: Vec<TransportAddr> = relays
            .iter()
            .map(|(url, _)| TransportAddr::Relay(url.clone()))
            .collect();
        addrs.extend(
            ticket
                .entry
                .direct_addrs
                .iter()
                .copied()
                .map(TransportAddr::Ip),
        );
        ensure!(
            !addrs.is_empty(),
            "this invite carries no address of its entry Node"
        );
        let config = NetworkConfig {
            relay_paths_only: node.config.network.relay_paths_only,
            ..NetworkConfig::default()
        };
        let link = Net::bind(&node.identity.secret_key_hex, &config, 0, relays).await?;
        link.learn(&EndpointAddr::from_parts(net::endpoint_id(&entry)?, addrs))
            .await;
        Ok(Self { net: link, entry })
    }

    async fn call(&self, method: &str, params: Value) -> Result<Value> {
        let request = json!({"method":method,"params":params});
        let reply = self
            .net
            .request(&self.entry, net::JOIN, &request, CALL_TIMEOUT)
            .await?;
        Ok(reply["result"].clone())
    }
}

/// `vibeface join`: this Node asks the ticket's entry to admit it and waits for approval.
pub async fn join_network(dir: &Path, link: &str, name: &str) -> Result<NetworkState> {
    let (_, ticket) = parse_join_link(link)?;
    let node = Runtime::open(dir).await?;
    ensure!(
        !node.has_membership().await?,
        "this Node already belongs to a personal network"
    );
    let path = dir.join("join.json");
    let pending = match std::fs::read(&path) {
        Ok(bytes) => serde_json::from_slice::<PendingJoin>(&bytes)?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => PendingJoin {
            link: link.trim().to_owned(),
            name: name.to_owned(),
            nonce: crate::identity::local::random_hex(32),
        },
        Err(error) => return Err(error.into()),
    };
    ensure!(
        pending.link == link.trim() && pending.name == name,
        "another join is in progress; delete join.json to abandon it"
    );
    crate::config::private_write(&path, &serde_json::to_vec(&pending)?)?;
    let device = node.identity.device_id()?;
    let client = JoinLink::open(&node, &ticket).await?;
    let request: JoinRequest = serde_json::from_value(
        client
            .call(
                "join",
                json!({"secret":hex::encode(ticket.secret),"public_key":device,"kind":IdentityKind::Node,"name":name,"commit":join_commit(&device, &pending.nonce).map_err(anyhow::Error::msg)?}),
            )
            .await?,
    )?;
    ensure!(
        request.invite_id == invite_id(&ticket.invite_hash()).map_err(anyhow::Error::msg)?,
        "entry answered for another invite"
    );
    let join_id = request.join_id;
    println!("已提交加入请求，等待管理设备批准。");
    let deadline = Instant::now() + Duration::from_secs(11 * 60);
    let mut shown = false;
    loop {
        ensure!(
            Instant::now() < deadline,
            "approval timed out; run the same command again to resume"
        );
        let status = client
            .call("join_status", json!({"join_id":join_id}))
            .await?;
        let request: JoinRequest = serde_json::from_value(status.clone())?;
        match request.status {
            JoinStatus::Joined => {
                let state: NetworkState = from_field(&status, "state")?;
                ensure!(
                    state.genesis.network_id == request.network_id,
                    "entry returned another personal network"
                );
                state.project()?.node(&device)?;
                node.install_membership(state.clone()).await?;
                let addresses: Vec<net::SignedAddress> =
                    serde_json::from_value(status["addresses"].clone()).unwrap_or_default();
                node.merge_addresses(&request.network_id, addresses).await?;
                client.net.close().await;
                let _ = std::fs::remove_file(&path);
                return Ok(state);
            }
            JoinStatus::Rejected | JoinStatus::Expired => {
                let _ = std::fs::remove_file(&path);
                bail!(
                    "join request {:?}: {}",
                    request.status,
                    request.reason.as_deref().unwrap_or("expired")
                );
            }
            JoinStatus::Confirming if !shown => {
                let (admin, admin_nonce) = request
                    .admin_id
                    .as_deref()
                    .zip(request.admin_nonce.as_deref())
                    .context("entry omitted the manager nonce")?;
                let code = short_code(
                    &request.network_id,
                    &request.invite_id,
                    &device,
                    admin,
                    admin_nonce,
                    &pending.nonce,
                )
                .map_err(anyhow::Error::msg)?;
                println!("短码：{code}。请在管理设备上核对相同后批准。");
                client
                    .call(
                        "join_status",
                        json!({"join_id":join_id,"reveal":pending.nonce}),
                    )
                    .await?;
                shown = true;
            }
            _ => tokio::time::sleep(Duration::from_secs(2)).await,
        }
    }
}

fn text<'a>(value: &'a Value, key: &str) -> Result<&'a str> {
    let text = value[key]
        .as_str()
        .with_context(|| format!("{key} is required"))?;
    ensure!(
        !text.is_empty() && text.len() <= 2048 && !text.chars().any(char::is_control),
        "invalid {key}"
    );
    Ok(text)
}
fn from_field<T: serde::de::DeserializeOwned>(value: &Value, key: &str) -> Result<T> {
    serde_json::from_value(
        value
            .get(key)
            .cloned()
            .with_context(|| format!("{key} is required"))?,
    )
    .with_context(|| format!("invalid {key}"))
}
fn secure<T>(result: std::result::Result<T, String>) -> Result<T> {
    result.map_err(anyhow::Error::msg)
}
fn validate_request(request: &Request) -> Result<()> {
    ensure!(
        !request.id.is_empty()
            && request.id.len() <= 128
            && request
                .id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"-_.:".contains(&b)),
        "invalid request ID"
    );
    ensure!(
        !request.method.is_empty() && request.method.len() <= 64 && request.params.is_object(),
        "invalid client method or params"
    );
    Ok(())
}
async fn send_frame(socket: &mut ClientSocket, record: Vec<u8>) -> Result<()> {
    tokio::time::timeout(
        HANDSHAKE_TIMEOUT,
        socket.send(Message::Binary(record.into())),
    )
    .await
    .context("client send timed out")?
}
async fn next_binary(socket: &mut ClientSocket) -> Result<Vec<u8>> {
    while let Some(frame) = socket.next().await {
        match frame? {
            Message::Binary(bytes) => {
                ensure!(bytes.len() <= 65_535, "client frame too large");
                return Ok(bytes.to_vec());
            }
            Message::Text(text) => bail!("Gateway refused the connection: {text}"),
            Message::Ping(_) | Message::Pong(_) => {}
            _ => bail!("client channel accepts only encrypted binary frames"),
        }
    }
    bail!("client disconnected")
}
async fn send_value<T: Serialize>(
    socket: &mut ClientSocket,
    channel: &mut Channel,
    value: &T,
) -> Result<()> {
    let bytes = serde_json::to_vec(value)?;
    for record in secure(channel.seal_message(&bytes))? {
        send_frame(socket, record).await?;
    }
    Ok(())
}
async fn receive_value(socket: &mut ClientSocket, channel: &mut Channel) -> Result<Vec<u8>> {
    loop {
        if let Some(value) = secure(channel.open_record(&next_binary(socket).await?))? {
            return Ok(value);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::identity::Member;

    #[tokio::test]
    async fn node_settings_require_management_and_apply_without_restarting_or_losing_other_configuration()
    -> Result<()> {
        use crate::{
            config::{self, Config, ManagerSettings},
            model::NodeRole,
        };
        let dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../.artifacts")
            .join(format!("node-settings-{}", uuid::Uuid::new_v4()));
        config::private_dir(&dir)?;
        let mut config = Config::local("127.0.0.1:4790".parse()?, vec![], false);
        config.roles = vec![NodeRole::Relay, NodeRole::Replica, NodeRole::Scheduler];
        config.codex_socket = Some(dir.join("unavailable-codex.sock"));
        config.save(&dir)?;
        config::private_write(&dir.join("data-epoch.json"), br#"{"data_epoch":7}"#)?;
        config::private_write(
            &dir.join("identity.json"),
            &serde_json::to_vec(&LocalIdentity::generate())?,
        )?;
        let node = Arc::new(Runtime::open(&dir).await?);
        let root = LocalIdentity::generate();
        let admin = LocalIdentity::generate();
        let client = LocalIdentity::generate();
        let mut state = NetworkState::new(Genesis::create(
            &root,
            Member::client(&admin, "admin", true)?,
            Member::node(&node.identity, "node")?,
        )?)?;
        state.append(GovernanceEvent::grant(
            &admin,
            &state.project()?,
            Member::client(&client, "browser", false)?,
        )?)?;
        let network = state.genesis.network_id.clone();
        node.install_membership(state).await?;
        let session = |identity: &LocalIdentity| -> Result<Session> {
            Ok(Session {
                device_id: identity.device_id()?,
                cookie: None,
                subscribed: false,
                observation: None,
            })
        };
        for identity in [&client, &LocalIdentity::generate()] {
            let mut unauthorized = session(identity)?;
            assert!(
                dispatch(&node, &mut unauthorized, "node_settings", json!({}))
                    .await
                    .is_err()
            );
            assert!(
                dispatch(
                    &node,
                    &mut unauthorized,
                    "configure_node",
                    json!({"roots":[],"manager":null})
                )
                .await
                .is_err()
            );
        }
        let mut manager_session = session(&admin)?;
        assert_eq!(
            dispatch(&node, &mut manager_session, "info", json!({})).await?["connection_entry"],
            json!({"address":"http://127.0.0.1:4790","device_id":node.identity.device_id()?})
        );
        let initial = dispatch(&node, &mut manager_session, "node_settings", json!({})).await?;
        assert_eq!(initial["settings"], json!({"roots":[],"manager":null}));
        let before = std::fs::read(dir.join("config.json"))?;
        for invalid in [
            json!({"roots":["relative-project"],"manager":null}),
            json!({"roots":[],"manager":null,"codex_bin":"arbitrary"}),
        ] {
            assert!(
                dispatch(&node, &mut manager_session, "configure_node", invalid)
                    .await
                    .is_err()
            );
            assert_eq!(std::fs::read(dir.join("config.json"))?, before);
            assert!(node.settings_snapshot().roots.is_empty());
        }
        let project = dir.join("project");
        let workspace = std::path::PathBuf::from(
            initial["defaults"]["workspace"]
                .as_str()
                .context("default workspace missing")?,
        );
        config::private_dir(&project)?;
        assert!(!workspace.exists());
        let mut latest = Config::load(&dir)?;
        latest.network.relays.push("https://relay.example/".into());
        latest.daily_decision_limit = 11;
        latest.save(&dir)?;
        let settings = crate::runtime::NodeSettings {
            roots: vec![project.clone(), project.join(".")],
            manager: Some(ManagerSettings {
                provider: "cursor_sdk".into(),
                workspace: workspace.clone(),
                model: "auto".into(),
                cursor_sdk_bin: dir.join("missing-cursor-sdk"),
                node_bin: "node".into(),
                codex_sdk_script: dir.join("sidecars/codex-manager.mjs"),
            }),
        };
        let mut missing_workspace = settings.clone();
        missing_workspace.manager.as_mut().unwrap().workspace =
            dir.join("custom-workspace-not-created");
        assert!(
            dispatch(
                &node,
                &mut manager_session,
                "configure_node",
                serde_json::to_value(missing_workspace)?
            )
            .await
            .is_err()
        );
        assert!(!dir.join("custom-workspace-not-created").exists());
        std::os::unix::fs::symlink(&project, &workspace)?;
        assert!(
            dispatch(
                &node,
                &mut manager_session,
                "configure_node",
                serde_json::to_value(&settings)?
            )
            .await
            .is_err()
        );
        std::fs::remove_file(&workspace)?;
        let saved = dispatch(
            &node,
            &mut manager_session,
            "configure_node",
            serde_json::to_value(&settings)?,
        )
        .await?;
        assert!(workspace.is_dir());
        assert_eq!(saved["settings"]["roots"], json!([project.canonicalize()?]));
        assert_eq!(saved["readiness"]["manager"], "missing_prerequisites");
        assert_eq!(saved["readiness"]["scheduler_available"], true);
        assert_eq!(saved["readiness"]["missing"], json!(["Cursor SDK Bridge"]));
        assert_eq!(node.info()["manager_available"], true);
        let nodes = node
            .rpc(
                Access {
                    network_id: network,
                    human: true,
                    conversation_id: None,
                    manager: None,
                },
                json!({"method":"nodes"}),
            )
            .await?;
        assert_eq!(
            nodes["nodes"][0]["capabilities"]["roots"],
            json!([project.canonicalize()?])
        );
        assert_eq!(nodes["nodes"][0]["capabilities"]["manager"], "cursor_sdk");
        let persisted = Config::load(&dir)?;
        assert_eq!(persisted.network.relays, latest.network.relays);
        assert_eq!(persisted.daily_decision_limit, 11);
        assert_eq!(
            dispatch(
                &node,
                &mut manager_session,
                "configure_node",
                serde_json::to_value(&settings)?
            )
            .await?["settings"],
            saved["settings"]
        );
        let mut codex_settings = settings.clone();
        codex_settings.manager.as_mut().unwrap().provider = "codex_sdk".into();
        dispatch(
            &node,
            &mut manager_session,
            "configure_node",
            serde_json::to_value(&codex_settings)?,
        )
        .await?;
        dispatch(
            &node,
            &mut manager_session,
            "configure_node",
            serde_json::to_value(&settings)?,
        )
        .await?;
        drop(node);
        let reopened = Runtime::open(&dir).await?;
        assert_eq!(
            serde_json::to_value(reopened.settings_snapshot())?,
            saved["settings"]
        );
        drop(reopened);
        let mut restricted = Config::load(&dir)?;
        restricted.roles = vec![NodeRole::Relay, NodeRole::Replica];
        restricted.save(&dir)?;
        let restricted = Runtime::open(&dir).await?;
        assert_eq!(
            restricted.node_settings().await?["readiness"]["manager"],
            "role_unavailable"
        );
        assert_eq!(restricted.info()["manager_available"], false);
        drop(restricted);
        std::fs::remove_dir_all(dir)?;
        Ok(())
    }

    #[tokio::test]
    async fn local_browser_access_requires_a_launcher_and_preserves_membership_permissions()
    -> Result<()> {
        use tokio_tungstenite::tungstenite::client::IntoClientRequest;

        async fn connect_local(
            origin: &str,
            target: &str,
            cookie: &str,
            identity: &LocalIdentity,
        ) -> Result<Client> {
            let endpoint = origin.replace("http://", "ws://") + "/api/local/ws";
            let mut request = endpoint.into_client_request()?;
            request.headers_mut().insert("cookie", cookie.parse()?);
            let (socket, _) = tokio_tungstenite::connect_async(request).await?;
            Client::from_socket(ClientSocket::Client(Box::new(socket)), identity, target).await
        }

        async fn browser_node(
            dir: &std::path::Path,
            manager: &LocalIdentity,
        ) -> Result<(Arc<Runtime>, LocalIdentity)> {
            crate::config::private_dir(dir)?;
            let reservation = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
            let local = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
            let mut config = crate::config::Config::local(reservation.local_addr()?, vec![], false);
            config.local_listen = local.local_addr()?;
            drop((reservation, local));
            config.save(dir)?;
            crate::config::private_write(
                &dir.join("identity.json"),
                &serde_json::to_vec(&LocalIdentity::generate())?,
            )?;
            crate::config::private_write(&dir.join("data-epoch.json"), br#"{"data_epoch":7}"#)?;
            let node = Arc::new(Runtime::open(dir).await?);
            let root = LocalIdentity::generate();
            node.install_membership(NetworkState::new(Genesis::create(
                &root,
                Member::client(manager, "management browser", true)?,
                Member::node(&node.identity, "local device")?,
            )?)?)
            .await?;
            Ok((node, root))
        }

        let dir =
            std::env::temp_dir().join(format!("vibeface-local-browser-{}", uuid::Uuid::new_v4()));
        let manager = LocalIdentity::generate();
        let browser = LocalIdentity::generate();
        let (node, root) = browser_node(&dir, &manager).await?;
        let listen = node.config.listen;
        let serving = node.clone();
        let server = tokio::spawn(async move { crate::api::serve(serving).await });
        let mut second_server = None;
        let result: Result<()> = async {
            tokio::time::timeout(Duration::from_secs(5), async {
                while !dir.join("local-access.json").exists() { tokio::time::sleep(Duration::from_millis(10)).await; }
            }).await?;
            let launcher: crate::local_access::Launcher = serde_json::from_slice(&std::fs::read(dir.join("local-access.json"))?)?;
            let http = reqwest::Client::builder().timeout(Duration::from_secs(5)).build()?;
            assert_eq!(http.get(format!("http://{listen}/api/local/info")).header("Forwarded", "for=127.0.0.1;host=localhost").send().await?.status(), 404);
            let info: Value = http.get(format!("{}/api/local/info", launcher.origin)).send().await?.error_for_status()?.json().await?;
            assert_eq!(info["has_session"], false);
            let denied = http.post(format!("{}/api/local/launch", launcher.origin)).header("Origin", &launcher.origin).header("X-Forwarded-For", "127.0.0.1").json(&json!({})).send().await?;
            assert_eq!(denied.status(), 401);
            assert!(!denied.headers().contains_key("set-cookie"));
            assert_eq!(http.get(format!("{}/api/local/ws", launcher.origin)).header("Connection", "Upgrade").header("Upgrade", "websocket").header("Sec-WebSocket-Version", "13").header("Sec-WebSocket-Key", "dGhlIHNhbXBsZSBub25jZQ==").send().await?.status(), 401);

            let mut cookies = vec![];
            for identity in [&browser, &manager] {
                let launch: Value = http.post(format!("{}/api/local/launch", launcher.origin)).bearer_auth(&launcher.capability).json(&json!({})).send().await?.error_for_status()?.json().await?;
                let input = json!({"token":launch["token"],"node_id":launcher.node_id,"device_id":identity.device_id()?});
                let exchanged = http.post(format!("{}/api/local/exchange", launcher.origin)).header("Origin", &launcher.origin).json(&input).send().await?.error_for_status()?;
                let cookie = exchanged.headers().get("set-cookie").context("session cookie missing")?.to_str()?;
                assert!(cookie.contains("HttpOnly") && cookie.contains("SameSite=Strict") && cookie.contains("Path=/api/local"));
                cookies.push(cookie.split(';').next().context("session cookie missing")?.to_owned());
                assert_eq!(http.post(format!("{}/api/local/exchange", launcher.origin)).header("Origin", &launcher.origin).json(&input).send().await?.status(), 401);
            }
            let second_dir = dir.join("second");
            let (second, _) = browser_node(&second_dir, &manager).await?;
            second_server = Some(tokio::spawn(async move { crate::api::serve(second).await }));
            tokio::time::timeout(Duration::from_secs(5), async {
                while !second_dir.join("local-access.json").exists() { tokio::time::sleep(Duration::from_millis(10)).await; }
            }).await?;
            let second_launcher: crate::local_access::Launcher = serde_json::from_slice(&std::fs::read(second_dir.join("local-access.json"))?)?;
            let first_origin = reqwest::Url::parse(&launcher.origin)?;
            let second_origin = reqwest::Url::parse(&second_launcher.origin)?;
            assert_eq!(first_origin.host_str(), Some("127.0.0.1"));
            assert_eq!(second_origin.host_str(), first_origin.host_str());
            assert_ne!(second_origin.port(), first_origin.port());
            let launch: Value = http.post(format!("{}/api/local/launch", second_launcher.origin)).bearer_auth(&second_launcher.capability).json(&json!({})).send().await?.error_for_status()?.json().await?;
            let input = json!({"token":launch["token"],"node_id":second_launcher.node_id,"device_id":browser.device_id()?});
            let exchanged = http.post(format!("{}/api/local/exchange", second_launcher.origin)).header("Origin", &second_launcher.origin).json(&input).send().await?.error_for_status()?;
            let second_cookie = exchanged.headers().get("set-cookie").context("second session cookie missing")?.to_str()?;
            assert!(second_cookie.contains("HttpOnly") && second_cookie.contains("SameSite=Strict") && second_cookie.contains("Path=/api/local"));
            let second_cookie = second_cookie.split(';').next().context("second session cookie missing")?;
            let mut jar = std::collections::BTreeMap::new();
            for cookie in [cookies[0].as_str(), second_cookie] {
                let (name, value) = cookie.split_once('=').context("invalid session cookie")?;
                jar.insert(name, value);
            }
            assert_eq!(jar.len(), 2, "same-host Nodes must retain separate cookies");
            let shared_cookies = jar.iter().map(|(name, value)| format!("{name}={value}")).collect::<Vec<_>>().join("; ");
            for origin in [&launcher.origin, &second_launcher.origin] {
                let info: Value = http.get(format!("{origin}/api/local/info")).header("Cookie", &shared_cookies).send().await?.error_for_status()?.json().await?;
                assert_eq!(info["has_session"], true);
                assert_eq!(info["device_id"], browser.device_id()?);
            }
            let (first_name, _) = cookies[0].split_once('=').context("invalid session cookie")?;
            let (_, second_value) = second_cookie.split_once('=').context("invalid second session cookie")?;
            let wrong_node_cookie = format!("{first_name}={second_value}");
            let info: Value = http.get(format!("{}/api/local/info", launcher.origin)).header("Cookie", &wrong_node_cookie).send().await?.error_for_status()?.json().await?;
            assert_eq!(info["has_session"], false);
            assert_eq!(http.get(format!("{}/api/local/ws", launcher.origin)).header("Cookie", &wrong_node_cookie).header("Connection", "Upgrade").header("Upgrade", "websocket").header("Sec-WebSocket-Version", "13").header("Sec-WebSocket-Key", "dGhlIHNhbXBsZSBub25jZQ==").send().await?.status(), 401);
            let mut second_local = connect_local(&second_launcher.origin, &second_launcher.node_id, &shared_cookies, &browser).await?;
            assert_eq!(second_local.call("authenticate", json!({})).await?["principal"]["source"], "local_session");
            let mut local = connect_local(&launcher.origin, &launcher.node_id, &shared_cookies, &browser).await?;
            let auth = local.call("authenticate", json!({})).await?;
            assert_eq!(auth["principal"]["source"], "local_session");
            assert_eq!(auth["principal"]["can_manage"], false);
            assert!(!auth["view"]["members"].as_array().context("members missing")?.iter().any(|member| member["device_id"] == browser.device_id().unwrap()));
            assert!(local.call("join_status", json!({})).await.is_err());
            let local_project = dir.join("local-project");
            crate::config::private_dir(&local_project)?;
            let settings = local.call("node_settings", json!({})).await?;
            assert_eq!(settings["settings"]["manager"], Value::Null);
            let saved = local.call("configure_node", json!({"roots":[local_project],"manager":null})).await?;
            assert_eq!(saved["settings"]["roots"], json!([local_project.canonicalize()?]));
            assert_eq!(local.call("authenticate", json!({})).await?["principal"]["can_manage"], false);
            let before = std::fs::read(dir.join("config.json"))?;
            assert!(node.configure_node(node.settings_snapshot(), &browser.device_id()?, Some(&"0".repeat(64))).await.is_err());
            assert_eq!(std::fs::read(dir.join("config.json"))?, before);
            let conversation = uuid::Uuid::new_v4().to_string();
            let network = node.membership_state().await?.genesis.network_id;
            let owner = browser.attest(&network, &conversation, crate::model::EventBody::ConversationOwner {
                scheduler_node: node.identity.device_id()?, epoch: 1, attestation: None })?;
            let created = local.call("rpc", json!({"method":"create_conversation","conversation_id":conversation,"title":"Local browser",
                "scheduler_node":node.identity.device_id()?,"owner_attestation":owner.attestation()})).await;
            assert!(created.is_err(), "a local session's key is not a member's signature");
            let conversations = local.call("rpc", json!({"method":"conversations"})).await?;
            assert!(conversations["conversations"].is_array());
            let mut wrong_key = connect_local(&launcher.origin, &launcher.node_id, &cookies[0], &LocalIdentity::generate()).await?;
            assert!(wrong_key.call("authenticate", json!({})).await.is_err());
            let mut signed = connect_local(&launcher.origin, &launcher.node_id, &cookies[1], &manager).await?;
            let auth = signed.call("authenticate", json!({})).await?;
            assert_eq!(auth["principal"]["source"], "membership");
            assert_eq!(auth["principal"]["can_manage"], true);
            let cookie = cookies[0].split_once('=').context("local cookie missing")?.1;
            let browser_id = browser.device_id()?;
            let mut local_session = Session {
                device_id: browser_id.clone(), cookie: Some(cookie.into()),
                subscribed: false, observation: None,
            };
            let save = node.config_save.lock().await;
            let mut queued_disable = Box::pin(dispatch(&node, &mut local_session, "local_access", json!({"enabled":false})));
            assert!(futures_util::poll!(queued_disable.as_mut()).is_pending());
            node.local_access.set_enabled(false)?;
            let after_close = std::fs::read(dir.join("config.json"))?;
            drop(save);
            assert!(queued_disable.await.is_err());
            assert_eq!(std::fs::read(dir.join("config.json"))?, after_close);
            assert!(!crate::config::Config::load(&dir)?.local_auto_access);
            let before = std::fs::read(dir.join("config.json"))?;
            assert!(node.configure_node(node.settings_snapshot(), &browser.device_id()?, Some(cookie)).await.is_err());
            assert_eq!(std::fs::read(dir.join("config.json"))?, before);
            assert!(local.call("rpc", json!({"method":"conversations"})).await.is_err());
            assert_eq!(second_local.call("authenticate", json!({})).await?["principal"]["source"], "local_session");
            assert_eq!(signed.call("authenticate", json!({})).await?["principal"]["can_manage"], true);
            signed.call("local_access", json!({"enabled":true})).await?;
            assert_eq!(http.get(format!("{}/api/local/ws", launcher.origin)).header("Cookie", &cookies[0]).header("Connection", "Upgrade").header("Upgrade", "websocket").header("Sec-WebSocket-Version", "13").header("Sec-WebSocket-Key", "dGhlIHNhbXBsZSBub25jZQ==").send().await?.status(), 401);
            node.local_access.set_enabled(false)?;
            let mut manager_session = Session {
                device_id: manager.device_id()?, cookie: None,
                subscribed: false, observation: None,
            };
            let save = node.config_save.lock().await;
            let mut queued_enable = Box::pin(dispatch(&node, &mut manager_session, "local_access", json!({"enabled":true})));
            assert!(futures_util::poll!(queued_enable.as_mut()).is_pending());
            let replacement_manager = LocalIdentity::generate();
            let mut state = node.membership_state().await?;
            let checkpoint = RootCheckpoint::create(&root, state.epoch() + 1, vec![
                Member::node(&node.identity, "local device")?,
                Member::client(&replacement_manager, "replacement manager", true)?,
            ])?;
            state.recover(checkpoint)?;
            node.install_membership(state).await?;
            let after_recovery = std::fs::read(dir.join("config.json"))?;
            drop(save);
            assert!(queued_enable.await.is_err());
            assert_eq!(std::fs::read(dir.join("config.json"))?, after_recovery);
            assert!(!node.local_access.enabled());
            node.local_access.set_enabled(true)?;
            let mut state = node.membership_state().await?;
            let node_id = node.identity.device_id()?;
            let token = node.local_access.issue_from_local_setup(&node_id, &state.genesis.network_id)?;
            let cookie = node.local_access.exchange(&token, &node_id, &state.genesis.network_id, &browser_id)?;
            let mut settings = node.settings_snapshot();
            settings.roots.clear();
            let save = node.config_save.lock().await;
            let mut queued_settings = Box::pin(node.configure_node(settings, &browser_id, Some(&cookie)));
            assert!(futures_util::poll!(queued_settings.as_mut()).is_pending());
            state.append(GovernanceEvent::revoke(&replacement_manager, &state.project()?, &node_id, 0)?)?;
            node.install_membership(state.clone()).await?;
            assert!(node.local_access.session(&cookie, &node_id, &state.genesis.network_id).is_ok());
            let after_revoke = std::fs::read(dir.join("config.json"))?;
            drop(save);
            assert!(queued_settings.await.is_err());
            assert_eq!(std::fs::read(dir.join("config.json"))?, after_revoke);
            assert_eq!(node.settings_snapshot().roots, vec![local_project.canonicalize()?]);
            Ok(())
        }.await;
        server.abort();
        let _ = server.await;
        if let Some(server) = second_server {
            server.abort();
            let _ = server.await;
        }
        drop(node);
        std::fs::remove_dir_all(&dir)?;
        result
    }

    /// Genesis, join with commitment and short code, manage grant, voided requests,
    /// revocation and root recovery, all over a real `/api/client` listener.
    #[tokio::test]
    async fn identity_rpcs_admit_exactly_the_confirmed_device_over_the_gateway() -> Result<()> {
        use tokio_tungstenite::tungstenite::client::IntoClientRequest;
        use vibeface_secure::join::{INVITE_DOMAIN, invite_hash};

        let dir = std::env::temp_dir().join(format!("vibeface-identity-{}", uuid::Uuid::new_v4()));
        crate::config::private_dir(&dir)?;
        let public = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let local = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let mut config = crate::config::Config::local(public.local_addr()?, vec![], false);
        config.local_listen = local.local_addr()?;
        config.allowed_origins = vec!["https://app.example".into()];
        drop((public, local));
        config.save(&dir)?;
        crate::config::private_write(
            &dir.join("identity.json"),
            &serde_json::to_vec(&LocalIdentity::generate())?,
        )?;
        crate::config::private_write(&dir.join("data-epoch.json"), br#"{"data_epoch":7}"#)?;
        let capability =
            crate::store::Store::open(dir.join("node.sqlite3"))?.bootstrap_capability()?;
        let node = Arc::new(Runtime::open(&dir).await?);
        let origin = node.config.public_url.clone();
        let node_id = node.identity.device_id()?;
        let server = tokio::spawn(crate::api::serve(node.clone()));
        let result: Result<()> = async {
            let connect = |identity: LocalIdentity| {
                let (origin, node_id) = (origin.clone(), node_id.clone());
                async move {
                    for _ in 0..100 {
                        match Client::connect(&origin, &identity, &node_id).await {
                            Ok(client) => return Ok(client),
                            Err(_) => tokio::time::sleep(Duration::from_millis(20)).await,
                        }
                    }
                    Client::connect(&origin, &identity, &node_id).await
                }
            };
            let (root, admin) = (LocalIdentity::generate(), LocalIdentity::generate());
            let mut manager = connect(admin.clone()).await?;
            assert_eq!(manager.call("info", json!({})).await?["initialized"], false);
            assert!(manager.call("directory", json!({})).await.is_err());
            let genesis = Genesis::create(&root, Member::client(&admin, "admin", true)?, Member::node(&node.identity, "node")?)?;
            let recover = json!({"genesis":genesis,"capability":capability.capability});
            assert!(manager.call("recover", json!({"genesis":genesis,"capability":"00".repeat(32)})).await.is_err());
            let created = manager.call("recover", recover.clone()).await?;
            assert_eq!(created["principal"]["can_manage"], true);
            assert_eq!(manager.call("recover", recover).await?["principal"], created["principal"]);
            let network = genesis.network_id.clone();

            let invite = |secret: [u8; 32]| -> Result<Value> {
                let mut record = InviteRecord {
                    network_id: network.clone(),
                    invite_hash: invite_hash(&secret),
                    issuer: admin.device_id()?,
                    expires_at_ms: crate::runtime::now_ms() + 60_000,
                    signature: String::new(),
                };
                record.signature = admin.sign_payload(INVITE_DOMAIN, &record.signing_payload().map_err(anyhow::Error::msg)?)?;
                Ok(json!({"record":record}))
            };
            let join = |device: &LocalIdentity, secret: [u8; 32], nonce: &str| -> Result<Value> {
                Ok(json!({"secret":hex::encode(secret),"public_key":device.device_id()?,"kind":"client","name":"phone","commit":join_commit(&device.device_id()?, nonce).map_err(anyhow::Error::msg)?}))
            };
            let code = |request: &Value, nonce: &str| -> Result<String> {
                short_code(&network, request["invite_id"].as_str().context("invite_id")?, request["device_id"].as_str().context("device")?,
                    request["admin_id"].as_str().context("admin")?, request["admin_nonce"].as_str().context("admin nonce")?, nonce)
                    .map_err(anyhow::Error::msg)
            };

            // A new browser joins and is granted management.
            let phone = LocalIdentity::generate();
            let join_id = manager.call("invite", invite([1; 32])?).await?["join_id"].clone();
            let mut joiner = connect(phone.clone()).await?;
            assert_eq!(joiner.call("authenticate", json!({})).await?["principal"], Value::Null);
            assert!(joiner.call("directory", json!({})).await.is_err());
            assert!(joiner.call("join", join(&LocalIdentity::generate(), [1; 32], &"aa".repeat(32))?).await.is_err(), "join key must be the channel key");
            let nonce = "11".repeat(32);
            let pending = joiner.call("join", join(&phone, [1; 32], &nonce)?).await?;
            assert_eq!((&pending["join_id"], &pending["status"]), (&join_id, &json!("pending")));
            assert_eq!(joiner.call("join", join(&phone, [1; 32], &nonce)?).await?["join_id"], join_id);
            let admin_nonce = "22".repeat(32);
            manager.call("approve", json!({"join_id":join_id,"admin_nonce":admin_nonce})).await?;
            let seen = joiner.call("join_status", json!({"join_id":join_id})).await?;
            assert_eq!(seen["status"], "confirming");
            let phone_code = code(&seen, &nonce)?;
            joiner.call("join_status", json!({"join_id":join_id,"reveal":nonce})).await?;
            let listed = manager.call("join_status", json!({})).await?;
            let request = &listed["requests"][0];
            assert_eq!(code(request, request["joiner_nonce"].as_str().context("revealed nonce")?)?, phone_code);
            let view = node.membership_state().await?.project()?;
            let grant = GovernanceEvent::grant(&admin, &view, Member::client(&phone, "phone", true)?)?;
            assert_eq!(manager.call("approve", json!({"join_id":join_id,"event":grant})).await?["status"], "joined");
            let joined = joiner.call("join_status", json!({"join_id":join_id})).await?;
            assert_eq!(joined["state"]["genesis"]["network_id"], network);
            assert_eq!(joiner.call("authenticate", json!({})).await?["principal"]["can_manage"], true);

            // A wrong reveal and an explicit rejection both void the request for good.
            let (liar, nonce) = (LocalIdentity::generate(), "33".repeat(32));
            let liar_id = manager.call("invite", invite([2; 32])?).await?["join_id"].clone();
            let mut liar_client = connect(liar.clone()).await?;
            liar_client.call("join", join(&liar, [2; 32], &nonce)?).await?;
            manager.call("approve", json!({"join_id":liar_id,"admin_nonce":"44".repeat(32)})).await?;
            let voided = liar_client.call("join_status", json!({"join_id":liar_id,"reveal":"55".repeat(32)})).await?;
            assert_eq!((&voided["status"], &voided["reason"]), (&json!("rejected"), &json!("commitment_mismatch")));
            assert!(liar_client.call("join_status", json!({"join_id":liar_id,"reveal":nonce})).await.is_err());
            let refused = LocalIdentity::generate();
            let refused_id = manager.call("invite", invite([3; 32])?).await?["join_id"].clone();
            connect(refused.clone()).await?.call("join", join(&refused, [3; 32], &nonce)?).await?;
            assert_eq!(manager.call("approve", json!({"join_id":refused_id,"reject":true})).await?["status"], "rejected");
            assert!(manager.call("approve", json!({"join_id":refused_id,"admin_nonce":"66".repeat(32)})).await.is_err());

            // Revocation takes effect on the open session.
            let view = node.membership_state().await?.project()?;
            let revoke = GovernanceEvent::revoke(&admin, &view, &phone.device_id()?, 0)?;
            assert!(joiner.call("revoke", json!({"event":revoke})).await.is_err(), "issuer must be the session");
            manager.call("revoke", json!({"event":revoke})).await?;
            assert!(joiner.call("directory", json!({})).await.is_err());
            assert_eq!(joiner.call("authenticate", json!({})).await?["revoked"], true);

            // Root recovery from a device that holds only the recovery key.
            let replacement = LocalIdentity::generate();
            let epoch = node.membership_state().await?.epoch() + 1;
            let checkpoint = RootCheckpoint::create(&root, epoch, vec![
                Member::client(&admin, "admin", true)?,
                Member::client(&replacement, "new laptop", true)?,
                Member::node(&node.identity, "node")?,
            ])?;
            let dropped = RootCheckpoint::create(&root, epoch, vec![Member::client(&replacement, "new laptop", true)?])?;
            let mut recovering = connect(replacement.clone()).await?;
            let expires = crate::runtime::now_ms() + 60_000;
            let read = |signer: &LocalIdentity| -> Result<Value> {
                let payload = format!(r#"{{"device_id":"{}","expires_at_ms":{expires},"network_id":"{network}"}}"#, replacement.device_id()?);
                Ok(json!({"expires_at_ms":expires,"signature":signer.sign_payload(RECOVERY_READ_DOMAIN, payload.as_bytes())?}))
            };
            assert!(recovering.call("recovery_state", read(&admin)?).await.is_err(), "only the root may read");
            assert_eq!(recovering.call("recovery_state", read(&root)?).await?["state"]["genesis"]["network_id"], network);
            assert!(recovering.call("recover", json!({"checkpoint":dropped})).await.is_err());
            let recovered = recovering.call("recover", json!({"checkpoint":checkpoint})).await?;
            assert_eq!(recovered["principal"]["can_manage"], true);
            assert_eq!(manager.call("authenticate", json!({})).await?["principal"]["epoch"], epoch);

            // A target outside the personal network is refused.
            assert!(Client::connect(&origin, &admin, &LocalIdentity::generate().device_id()?).await.is_err());

            // `/api/client` takes the configured cross-origin page; other paths stay same-origin.
            let ws = |page: &str| -> Result<_> {
                let mut request = format!("{}/api/client", origin.replace("http://", "ws://")).into_client_request()?;
                request.headers_mut().insert("origin", page.parse()?);
                Ok(request)
            };
            let (socket, _) = tokio_tungstenite::connect_async(ws("https://app.example")?).await?;
            let mut socket = ClientSocket::Client(Box::new(socket));
            socket.send(Message::Text(json!({"type":"connect","target":node_id}).to_string().into())).await?;
            let mut cross = Client::from_socket(socket, &admin, &node_id).await?;
            assert_eq!(cross.call("authenticate", json!({})).await?["principal"]["can_manage"], true);
            assert!(tokio_tungstenite::connect_async(ws("https://evil.example")?).await.is_err());
            let http = reqwest::Client::builder().no_proxy().build()?;
            let other = http.post(format!("{origin}/api/local/setup")).header("origin", "https://app.example").json(&json!({})).send().await?;
            assert_eq!(other.status(), 403);
            Ok(())
        }
        .await;
        server.abort();
        let _ = server.await;
        drop(node);
        std::fs::remove_dir_all(&dir)?;
        result
    }

    #[test]
    fn rpc_client_identity_is_bound_to_the_noise_principal() {
        let mut input = json!({"method":"notifications"});
        bind_client_identity(&mut input, "actual-client").unwrap();
        assert_eq!(input["client_id"], "actual-client");
        assert!(
            bind_client_identity(
                &mut json!({"method":"ack","client_id":"another-client"}),
                "actual-client"
            )
            .is_err()
        );
        assert!(bind_client_identity(&mut json!({"method":"operate","request":{"action":{"response":{"client_id":"business-data"}}}}), "actual-client").is_ok());
        assert!(
            bind_client_identity(
                &mut json!({"method":"message","client_id":"actual-client"}),
                "actual-client"
            )
            .is_ok()
        );
        assert!(
            bind_client_identity(
                &mut json!({"method":"message","attestation":{"device_id":"another-client","signature":""}}),
                "actual-client"
            )
            .is_err()
        );
    }

    #[test]
    fn channel_requests_require_bounded_ids_and_object_params() {
        let mut request = Request {
            id: "valid-request".into(),
            method: "info".into(),
            params: json!({}),
        };
        assert!(validate_request(&request).is_ok());
        request.params = json!("untyped payload");
        assert!(validate_request(&request).is_err());
        request.params = json!({});
        request.id = "\n".into();
        assert!(validate_request(&request).is_err());
    }
}
