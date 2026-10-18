//! Noise proves device possession. Every business request carries an independent Invocation.

use crate::{
    crypto::LocalIdentity,
    identity::node_execution_policy::{NodeReadRequest, NodeReadResponse},
    network::{NetworkHub, NetworkSocket, Service},
    runtime::{Access, Runtime},
};
use anyhow::{Context, Result, bail, ensure};
use axum::extract::ws::Message;
use futures_util::{SinkExt, StreamExt};
use serde::{
    Deserialize, Deserializer, Serialize,
    de::{self, MapAccess, SeqAccess, Visitor},
};
use serde_json::{Value, json};
use std::{
    collections::HashSet,
    sync::Arc,
    time::{Duration, Instant},
};
use vibeface_secure::authorization::{
    Actor, DeviceId, Invocation, MAX_PROOF_BYTES, PublicKey, Signed, TransportEvidence,
    VerifiedClientAuthorization,
    execution_observation::ExecutionObservationRequest,
    node_execution_policy::{ActualPolicyNoisePeer, VerifiedTaskExecutionTrust},
};
use vibeface_secure::{Channel, Handshake};

const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
const CALL_TIMEOUT: Duration = Duration::from_secs(120);
const IDLE_TIMEOUT: Duration = Duration::from_secs(90);
const HEARTBEAT: Duration = Duration::from_secs(20);
const NODE_POLICY_READ_METHOD: &str = "node_policy_read";

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Request {
    id: String,
    method: String,
    #[serde(default)]
    params: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    invocation: Option<Signed<Invocation>>,
}
#[derive(Serialize)]
struct Response {
    id: String,
    result: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}
struct Session {
    public_key: String,
    node_policy_read_only: bool,
    authorization: Option<VerifiedClientAuthorization>,
    subscription: Option<Access>,
    remote_subscription: Option<RemoteSubscription>,
}
impl Session {
    fn select_method(&mut self, method: &str) -> Result<()> {
        if method == NODE_POLICY_READ_METHOD {
            ensure!(
                self.authorization.is_none()
                    && self.subscription.is_none()
                    && self.remote_subscription.is_none(),
                "node policy reads require an independent unauthenticated session"
            );
            self.node_policy_read_only = true;
        } else {
            ensure!(
                !self.node_policy_read_only,
                "node policy reader sessions cannot change purpose"
            );
        }
        Ok(())
    }
}
struct RemoteSubscription {
    body: Value,
    invocation: Signed<Invocation>,
    cursor: String,
}

pub struct Client {
    socket: NetworkSocket,
    channel: Channel,
}

pub(crate) struct NodePolicyReadClient {
    client: Client,
    noise_peer: ActualPolicyNoisePeer,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct NodePolicyReadReply {
    id: String,
    result: Option<NodeReadResponse>,
    error: Option<String>,
}

impl NodePolicyReadClient {
    pub(crate) async fn connect(
        network: &NetworkHub,
        identity: &LocalIdentity,
        trust: &VerifiedTaskExecutionTrust,
    ) -> Result<Self> {
        let target = &trust.selection().target_public_key;
        let target_node_id = crate::crypto::fingerprint(target.as_str())?;
        let socket = network.dial(&target_node_id, Service::Client).await?;
        let (client, noise_peer) =
            Client::from_socket_with_policy_peer(socket, identity, &target_node_id).await?;
        ensure!(
            noise_peer.public_key() == target,
            "node policy Noise peer differs from the approved target key"
        );
        Ok(Self { client, noise_peer })
    }

    pub(crate) fn actual_noise_peer(&self) -> &ActualPolicyNoisePeer {
        &self.noise_peer
    }

    pub(crate) async fn read(&mut self, request: &NodeReadRequest) -> Result<NodeReadResponse> {
        let id = uuid::Uuid::new_v4().to_string();
        let wire = serde_json::to_vec(&Request {
            id: id.clone(),
            method: NODE_POLICY_READ_METHOD.to_owned(),
            params: serde_json::to_value(request)?,
            invocation: None,
        })?;
        require_node_policy_carrier(wire.len(), 0)?;
        tokio::time::timeout(CALL_TIMEOUT, async {
            for record in secure(self.client.channel.seal_message(&wire))? {
                send_frame(&mut self.client.socket, record).await?;
            }
            let bytes = receive_value(&mut self.client.socket, &mut self.client.channel).await?;
            require_node_policy_carrier(wire.len(), bytes.len())?;
            serde_json::from_slice::<UniqueJsonKeys>(&bytes)?;
            let response: NodePolicyReadReply = serde_json::from_slice(&bytes)?;
            ensure!(
                response.id == id,
                "node policy response changed its request ID"
            );
            let response = match (response.result, response.error) {
                (Some(response), None) => response,
                (None, Some(error)) => bail!("{error}"),
                _ => bail!("invalid node policy response"),
            };
            ensure!(
                matches!(
                    (request, &response),
                    (NodeReadRequest::Prepare(_), NodeReadResponse::Snapshot(_))
                        | (NodeReadRequest::Page(_), NodeReadResponse::Page(_))
                        | (NodeReadRequest::Finish(_), NodeReadResponse::Witness(_))
                ),
                "node policy response changed its read stage"
            );
            Ok(response)
        })
        .await
        .context("node policy read timed out")?
    }
}

fn require_node_policy_carrier(request_bytes: usize, response_bytes: usize) -> Result<()> {
    ensure!(
        request_bytes.saturating_add(response_bytes) <= MAX_PROOF_BYTES,
        "node policy request and response exceed 64 KiB"
    );
    Ok(())
}

impl Client {
    pub async fn connect(
        url: &str,
        identity: &LocalIdentity,
        target: &str,
        relay: bool,
    ) -> Result<Self> {
        let endpoint = client_endpoint(url)?;
        let socket = if relay {
            crate::network::connect_relay(&endpoint, target, Service::Client).await?
        } else {
            crate::network::connect_direct(&endpoint, Service::Client).await?
        };
        Self::from_socket(socket, identity, target).await
    }
    async fn from_socket(
        socket: NetworkSocket,
        identity: &LocalIdentity,
        target: &str,
    ) -> Result<Self> {
        let (client, _) = Self::from_socket_with_policy_peer(socket, identity, target).await?;
        Ok(client)
    }
    async fn from_socket_with_policy_peer(
        mut socket: NetworkSocket,
        identity: &LocalIdentity,
        target: &str,
    ) -> Result<(Self, ActualPolicyNoisePeer)> {
        let (channel, noise_peer) = tokio::time::timeout(HANDSHAKE_TIMEOUT, async {
            let mut handshake = secure(Handshake::new(
                true,
                &identity.secret_key_hex,
                Some(target.to_owned()),
                b"vibeface:client:v2",
            ))?;
            send_frame(&mut socket, secure(handshake.write(&[]))?).await?;
            let second = next_binary(&mut socket).await?;
            ensure!(
                secure(handshake.read(&second))?.is_empty(),
                "unexpected client handshake payload"
            );
            send_frame(&mut socket, secure(handshake.write(&[]))?).await?;
            let noise_peer = ActualPolicyNoisePeer::from_finished_handshake(&handshake)?;
            Ok::<_, anyhow::Error>((secure(handshake.finish())?, noise_peer))
        })
        .await
        .context("client handshake timed out")??;
        Ok((Self { socket, channel }, noise_peer))
    }
    pub async fn call(&mut self, method: &str, params: Value) -> Result<Value> {
        let id = uuid::Uuid::new_v4().to_string();
        let request = Request {
            id: id.clone(),
            method: method.to_owned(),
            params,
            invocation: None,
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

pub async fn serve(node: Arc<Runtime>, mut socket: NetworkSocket) -> Result<()> {
    let (mut channel, public_key) = tokio::time::timeout(HANDSHAKE_TIMEOUT, async {
        let mut handshake = secure(Handshake::new(
            false,
            &node.identity.secret_key_hex,
            None,
            b"vibeface:client:v2",
        ))?;
        ensure!(
            secure(handshake.read(&next_binary(&mut socket).await?))?.is_empty(),
            "unexpected first client payload"
        );
        send_frame(&mut socket, secure(handshake.write(&[]))?).await?;
        ensure!(
            secure(handshake.read(&next_binary(&mut socket).await?))?.is_empty(),
            "unexpected final client payload"
        );
        let public_key = handshake
            .remote_public_key()
            .context("client public key missing")?;
        Ok::<_, anyhow::Error>((secure(handshake.finish())?, public_key))
    })
    .await
    .context("client handshake timed out")??;
    let mut session = Session {
        public_key,
        node_policy_read_only: false,
        authorization: None,
        subscription: None,
        remote_subscription: None,
    };
    let mut events = node.events.subscribe();
    let mut heartbeat = tokio::time::interval(HEARTBEAT);
    heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut subscription_tick = tokio::time::interval(Duration::from_secs(5));
    subscription_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
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
                            let request = parse_request(&plaintext).context("invalid client request")?;
                            validate_request(&request)?;
                            if request.method == NODE_POLICY_READ_METHOD {
                                require_node_policy_carrier(plaintext.len(), 0)?;
                            }
                            let result = dispatch(&node, &mut session, request).await;
                            let (id, result) = result;
                            let response = match result {
                                Ok(result) => Response { id, result, error: None },
                                Err(error) => Response { id, result: Value::Null, error: Some(error.to_string()) },
                            };
                            if session.node_policy_read_only {
                                require_node_policy_carrier(plaintext.len(), serde_json::to_vec(&response)?.len())?;
                            }
                            send_value(&mut socket, &mut channel, &response).await?;
                        } else { partial_started.get_or_insert_with(Instant::now); }
                    }
                    Message::Ping(_) | Message::Pong(_) => {},
                    _ => bail!("client channel accepts only encrypted binary frames"),
                }
            }
            event = events.recv(), if session.subscription.is_some() => {
                match event {
                    Ok(event) => {
                        let access = session.subscription.as_ref().context("subscription missing")?;
                        if node.subscription_allows(access, &event).await? {
                            send_value(&mut socket, &mut channel, &json!({"type":"changed"})).await?;
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                        session.subscription = None;
                        send_value(&mut socket, &mut channel, &json!({"type":"subscription_expired"})).await?;
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => return Ok(()),
                }
            }
            _ = subscription_tick.tick() => {
                let mut expired = false;
                if let Some(remote) = session.remote_subscription.as_mut() {
                    let client = session.authorization.as_ref().context("subscription authorization missing")?;
                    let result = tokio::time::timeout(Duration::from_secs(5), node.remote_subscription_cursor(
                        client, &remote.body, &remote.invocation, &session.public_key,
                    )).await;
                    match result {
                        Ok(Ok(cursor)) if cursor != remote.cursor => {
                            remote.cursor = cursor;
                            send_value(&mut socket, &mut channel, &json!({"type":"changed"})).await?;
                        }
                        Ok(Ok(_)) => {},
                        _ => expired = true,
                    }
                } else if let Some(access) = &session.subscription {
                    match node.refresh_subscription(access).await {
                        Ok(current) => session.subscription = Some(current),
                        Err(_) => expired = true,
                    }
                }
                if expired {
                    session.subscription = None;
                    session.remote_subscription = None;
                    send_value(&mut socket, &mut channel, &json!({"type":"subscription_expired"})).await?;
                }
            }
            _ = heartbeat.tick() => {
                ensure!(received.elapsed() < IDLE_TIMEOUT, "client channel idle");
                ensure!(partial_started.is_none_or(|at| at.elapsed() < IDLE_TIMEOUT), "client fragmented message timed out");
                tokio::time::timeout(HANDSHAKE_TIMEOUT, socket.send(Message::Ping(Vec::new().into()))).await.context("client heartbeat send timed out")??;
            }
        }
    }
}

async fn dispatch(
    node: &Arc<Runtime>,
    session: &mut Session,
    request: Request,
) -> (String, Result<Value>) {
    let id = request.id;
    let result = dispatch_request(
        node,
        session,
        &request.method,
        request.params,
        request.invocation,
    )
    .await;
    (id, result)
}

async fn dispatch_request(
    node: &Arc<Runtime>,
    session: &mut Session,
    method: &str,
    params: Value,
    invocation: Option<Signed<Invocation>>,
) -> Result<Value> {
    session.select_method(method)?;
    match method {
        NODE_POLICY_READ_METHOD => {
            ensure!(
                invocation.is_none(),
                "node policy reads do not accept an Invocation"
            );
            let request: NodeReadRequest = serde_json::from_value(params)?;
            let response = node
                .node_execution_policy_read(&session.public_key, request)
                .await?;
            Ok(serde_json::to_value(response)?)
        }
        "info" => Ok(node.info()),
        "authenticate" => {
            let (verified, response) = {
                let mut store = node.store.lock().await;
                store.authenticate_confirmed_client_policy(
                    &node.identity,
                    &session.public_key,
                    &params,
                    crate::runtime::now_ms() as u64,
                )?
            };
            session.subscription = None;
            session.remote_subscription = None;
            session.authorization = Some(verified);
            Ok(response)
        }
        "prepare_invocation" => {
            let client = session
                .authorization
                .as_ref()
                .context("authenticate before preparing a request")?;
            let intent = serde_json::from_value(params["intent"].clone())?;
            node.prepare_client_request(client, params["body"].clone(), intent, &session.public_key)
                .await
        }
        "rpc" | "subscribe" => {
            let client = session
                .authorization
                .as_ref()
                .context("authenticate before sending a request")?;
            if params["method"] == "execution_observation" {
                ensure!(
                    method == "rpc" && invocation.is_none(),
                    "execution observation requires its dedicated signed request"
                );
                let observation: Signed<ExecutionObservationRequest> =
                    serde_json::from_value(params["observation_request"].clone())?;
                let request = &observation.payload.request;
                ensure!(
                    request.actor == Actor::Account(client.scope().account_id.clone())
                        && request.device == client.scope().device_id
                        && request.resource.space == client.scope().space_id
                        && &request.audience == client.audience(),
                    "observation differs from the authenticated client scope"
                );
                if client.audience() != &node.local_audience()? {
                    return node
                        .forward_client_execution_observation(client, &params, &session.public_key)
                        .await;
                }
                return node
                    .observe_client_execution(
                        params,
                        TransportEvidence::DirectNoise {
                            device: DeviceId::from_key(&PublicKey::parse(&session.public_key)?),
                        },
                    )
                    .await;
            }
            ensure!(
                params["space_id"].as_str() == Some(client.scope().space_id.as_str()),
                "request belongs to another space"
            );
            let invocation = invocation.context("signed Invocation is required")?;
            ensure!(
                invocation.payload.grant_leaf == *client.grant_leaf(),
                "request uses another grant"
            );
            if client.audience() != &node.local_audience()? {
                if method == "subscribe" {
                    let cursor = node
                        .remote_subscription_cursor(
                            client,
                            &params,
                            &invocation,
                            &session.public_key,
                        )
                        .await?;
                    session.subscription = None;
                    session.remote_subscription = Some(RemoteSubscription {
                        body: params,
                        invocation,
                        cursor,
                    });
                    return Ok(json!({"subscribed":true}));
                }
                return node
                    .forward_client_request(client, &params, &invocation, &session.public_key)
                    .await;
            }
            let device = DeviceId::from_key(&PublicKey::parse(&session.public_key)?);
            let transport = if invocation.payload.presenter == device {
                TransportEvidence::DirectNoise { device }
            } else {
                TransportEvidence::GatewayNoise {
                    device,
                    gateway: DeviceId::from_key(&PublicKey::parse(
                        &node.identity.public_key_hex()?,
                    )?),
                }
            };
            let access = node
                .authorize_rpc(params.clone(), invocation, transport)
                .await?;
            if method == "subscribe" {
                node.validate_subscription(&access).await?;
                session.remote_subscription = None;
                session.subscription = Some(access);
                Ok(json!({"subscribed":true}))
            } else {
                node.rpc(access, params).await
            }
        }
        "identity" => {
            let method = params["method"]
                .as_str()
                .context("identity method is required")?;
            ensure!(
                method != "authenticate",
                "use the authenticate channel method to bind this session"
            );
            node.identity_command(&session.public_key, method, &params["params"])
                .await
        }
        _ => bail!("unknown client method"),
    }
}

fn client_endpoint(value: &str) -> Result<String> {
    let mut url = reqwest::Url::parse(value)?;
    let scheme = match url.scheme() {
        "http" | "ws" => "ws",
        "https" | "wss" => "wss",
        _ => bail!("invalid client URL scheme"),
    };
    url.set_scheme(scheme)
        .map_err(|_| anyhow::anyhow!("invalid client URL"))?;
    ensure!(
        url.username().is_empty()
            && url.password().is_none()
            && url.query().is_none()
            && url.fragment().is_none(),
        "client URL must not contain credentials, query, or fragment"
    );
    ensure!(
        matches!(url.path(), "/" | "/api/network/ws"),
        "client URL must be a node base URL or network endpoint"
    );
    url.set_path("/api/network/ws");
    Ok(url.into())
}

fn secure<T>(result: std::result::Result<T, String>) -> Result<T> {
    result.map_err(anyhow::Error::msg)
}

struct UniqueJsonKeys;

impl<'de> Deserialize<'de> for UniqueJsonKeys {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        deserializer.deserialize_any(Self)
    }
}

impl<'de> Visitor<'de> for UniqueJsonKeys {
    type Value = Self;

    fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("JSON with unique object keys")
    }

    fn visit_bool<E>(self, _: bool) -> std::result::Result<Self, E> {
        Ok(Self)
    }

    fn visit_i64<E>(self, _: i64) -> std::result::Result<Self, E> {
        Ok(Self)
    }

    fn visit_u64<E>(self, _: u64) -> std::result::Result<Self, E> {
        Ok(Self)
    }

    fn visit_f64<E>(self, _: f64) -> std::result::Result<Self, E> {
        Ok(Self)
    }

    fn visit_str<E>(self, _: &str) -> std::result::Result<Self, E> {
        Ok(Self)
    }

    fn visit_unit<E>(self) -> std::result::Result<Self, E> {
        Ok(Self)
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut sequence: A) -> std::result::Result<Self, A::Error> {
        while sequence.next_element::<Self>()?.is_some() {}
        Ok(Self)
    }

    fn visit_map<A: MapAccess<'de>>(self, mut object: A) -> std::result::Result<Self, A::Error> {
        let mut keys = HashSet::new();
        while let Some(key) = object.next_key::<String>()? {
            if !keys.insert(key) {
                return Err(de::Error::custom("duplicate JSON object key"));
            }
            object.next_value::<Self>()?;
        }
        Ok(Self)
    }
}

fn parse_request(bytes: &[u8]) -> Result<Request> {
    serde_json::from_slice::<UniqueJsonKeys>(bytes)?;
    Ok(serde_json::from_slice(bytes)?)
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
async fn send_frame(socket: &mut NetworkSocket, record: Vec<u8>) -> Result<()> {
    tokio::time::timeout(
        HANDSHAKE_TIMEOUT,
        socket.send(Message::Binary(record.into())),
    )
    .await
    .context("client send timed out")?
}
async fn next_binary(socket: &mut NetworkSocket) -> Result<Vec<u8>> {
    while let Some(frame) = socket.next().await {
        match frame? {
            Message::Binary(bytes) => {
                ensure!(bytes.len() <= 65_535, "client frame too large");
                return Ok(bytes.to_vec());
            }
            Message::Ping(_) | Message::Pong(_) => {}
            _ => bail!("client channel accepts only encrypted binary frames"),
        }
    }
    bail!("client disconnected")
}
async fn send_value<T: Serialize>(
    socket: &mut NetworkSocket,
    channel: &mut Channel,
    value: &T,
) -> Result<()> {
    let bytes = serde_json::to_vec(value)?;
    for record in secure(channel.seal_message(&bytes))? {
        send_frame(socket, record).await?;
    }
    Ok(())
}
async fn receive_value(socket: &mut NetworkSocket, channel: &mut Channel) -> Result<Vec<u8>> {
    loop {
        if let Some(value) = secure(channel.open_record(&next_binary(socket).await?))? {
            return Ok(value);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn node_policy_reader_uses_real_noise_and_cannot_upgrade_its_session() -> Result<()> {
        use crate::{
            config::{self, Config},
            identity::{foreign_execution_policy::tests::unseeded_fixture, test_support::now},
            network::{MAX_FRAME_BYTES, NetworkConfig},
            store::Store,
        };
        use axum::{
            Router,
            extract::{State as AxumState, WebSocketUpgrade},
            routing::get,
        };
        use tokio::{net::TcpListener, task::JoinSet};
        use vibeface_secure::{
            Identity,
            authorization::{
                AUTH_VERSION, Nonce, ObjectHash,
                node_execution_policy::{
                    NodeExecutionPolicyReadIntent, NodeExecutionPolicyReadRequest,
                },
                remote_execution_policy::RemoteExecutionPolicyPageRequest,
            },
        };

        fn policy_counts(store: &Store) -> Result<(u64, u64, u64)> {
            Ok(store.connection.query_row(
                "SELECT (SELECT count(*) FROM policy_pins),
                    (SELECT count(*) FROM policy_journal),
                    (SELECT count(*) FROM current_policy_heads)",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )?)
        }

        async fn upgrade(
            AxumState(hub): AxumState<Arc<NetworkHub>>,
            ws: WebSocketUpgrade,
        ) -> impl axum::response::IntoResponse {
            ws.max_message_size(MAX_FRAME_BYTES)
                .max_frame_size(MAX_FRAME_BYTES)
                .on_upgrade(|socket| async move {
                    let _ = hub.accept(socket).await;
                })
        }

        let mut fixture = unseeded_fixture()?;
        let before_b = policy_counts(&fixture.store)?;
        let before_c = policy_counts(&fixture.target)?;
        let b_node_id = fixture.project.node.node_id()?;
        assert_eq!(
            fixture.target.connection.query_row(
                "SELECT count(*) FROM node_certificates WHERE node_id=?",
                [&b_node_id],
                |row| row.get::<_, u64>(0),
            )?,
            0
        );
        assert!(
            fixture
                .target
                .authorize_transport_node(&b_node_id, now())
                .is_err()
        );
        assert!(
            fixture
                .store
                .authorization_state()
                .account(&fixture.executor.account)
                .is_none()
        );
        let trust = fixture.store.approve_task_execution_policy(
            &fixture.project.node,
            &fixture.project.client.public_key_hex(),
            &fixture.intent,
            now(),
        )?;
        let original = &trust.original_intent().payload;
        let signer = Identity::from_secret_hex(&fixture.project.node.secret_key_hex)
            .map_err(anyhow::Error::msg)?;
        let request = NodeExecutionPolicyReadRequest::Current(Signed::sign(
            NodeExecutionPolicyReadIntent {
                version: AUTH_VERSION,
                source_project: original.source_selection.resource.clone(),
                selection: original.selection.clone(),
                context: original.context.clone(),
                expected_action: original.expected_action,
                purpose: original.purpose,
                execution_pins: original.execution_pins.clone(),
                nonce: Nonce::random()?,
                issued_at_ms: now(),
                expires_at_ms: original.expires_at_ms,
            },
            &signer,
        )?);

        let dir = &fixture.executor.directory;
        config::private_write(
            &dir.join("data-epoch.json"),
            &serde_json::to_vec(&config::data_format())?,
        )?;
        let mut cfg = Config::local(
            "127.0.0.1:4790".parse()?,
            vec![fixture.executor.project.clone()],
            false,
        );
        cfg.codex_socket = Some(dir.join("unavailable.sock"));
        cfg.codex_bin = dir.join("missing-codex");
        cfg.claude_bin = dir.join("missing-claude");
        cfg.save(dir)?;
        config::private_write(
            &dir.join("identity.json"),
            &serde_json::to_vec(&fixture.executor.node)?,
        )?;
        drop(fixture.executor.at(&dir.join("node.sqlite3"))?);
        let node = Arc::new(Runtime::open(dir).await?);
        *node.store.lock().await = fixture.target;

        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let url = format!("ws://{}/api/network/ws", listener.local_addr()?);
        let (target_network, mut incoming) = NetworkHub::new(
            fixture.executor.node.clone(),
            NetworkConfig {
                advertise_urls: vec![url.clone()],
                ..Default::default()
            },
        );
        let router = Router::new()
            .route("/api/network/ws", get(upgrade))
            .with_state(target_network);
        let mut tasks = JoinSet::new();
        tasks.spawn(async move {
            let _ = axum::serve(listener, router).await;
        });
        let server = node.clone();
        tasks.spawn(async move {
            while let Some(incoming) = incoming.recv().await {
                assert_eq!(incoming.service, Service::Client);
                let _ = serve(server.clone(), incoming.socket).await;
            }
        });
        let (network, _) = NetworkHub::new(
            fixture.project.node.clone(),
            NetworkConfig {
                bootstrap_urls: vec![url],
                ..Default::default()
            },
        );

        let target_key = &trust.selection().target_public_key;
        let socket = network
            .dial(&fixture.executor.node.node_id()?, Service::Client)
            .await?;
        let wrong_pin = Client::from_socket(socket, &fixture.project.node, target_key.as_str())
            .await
            .err()
            .context("a public key must not pass the Noise fingerprint gate")?;
        assert!(
            wrong_pin
                .to_string()
                .contains("unexpected Noise peer identity")
        );

        let mut reader =
            NodePolicyReadClient::connect(&network, &fixture.project.node, &trust).await?;
        assert_eq!(reader.actual_noise_peer().public_key(), target_key);
        let NodeReadResponse::Snapshot(snapshot) = reader
            .read(&NodeReadRequest::Prepare(request.clone()))
            .await?
        else {
            anyhow::bail!("expected a Node policy snapshot");
        };
        snapshot.verify_signature()?;
        assert_eq!(&snapshot.issuer, target_key);
        assert_eq!(snapshot.payload.request_hash, ObjectHash::body(&request)?);
        for (method, params) in [
            ("authenticate", json!({})),
            ("rpc", json!({})),
            ("identity", json!({"method":"status","params":{}})),
        ] {
            let error = reader
                .client
                .call(method, params)
                .await
                .err()
                .context("a policy reader must not upgrade its session")?;
            assert!(
                error.to_string().contains("cannot change purpose"),
                "{error}"
            );
        }

        let mut cursor = None;
        let mut previous = snapshot.hash()?;
        let mut page_count = 0;
        let mut entry_count = 0;
        loop {
            let NodeReadResponse::Page(page) = reader
                .read(&NodeReadRequest::Page(RemoteExecutionPolicyPageRequest {
                    snapshot_id: snapshot.payload.snapshot_id.clone(),
                    request_hash: snapshot.payload.request_hash.clone(),
                    cursor: cursor.clone(),
                }))
                .await?
            else {
                anyhow::bail!("expected a Node policy page");
            };
            page.verify_signature()?;
            assert_eq!(&page.issuer, target_key);
            assert_eq!(page.payload.snapshot_id, snapshot.payload.snapshot_id);
            assert_eq!(page.payload.request_hash, snapshot.payload.request_hash);
            assert_eq!(page.payload.page_index, page_count);
            assert_eq!(page.payload.request_cursor, cursor);
            assert_eq!(page.payload.previous_page_hash, previous);
            previous = page.hash()?;
            page_count += 1;
            entry_count += u64::from(page.payload.entry.is_some());
            if page.payload.complete {
                assert!(page.payload.next_cursor.is_none());
                break;
            }
            cursor = Some(
                page.payload
                    .next_cursor
                    .context("next policy page missing")?,
            );
            ensure!(page_count < 128, "Node policy stream did not finish");
        }
        let NodeReadResponse::Witness(witness) = reader
            .read(&NodeReadRequest::Finish(RemoteExecutionPolicyPageRequest {
                snapshot_id: snapshot.payload.snapshot_id.clone(),
                request_hash: snapshot.payload.request_hash.clone(),
                cursor: None,
            }))
            .await?
        else {
            anyhow::bail!("expected a Node policy witness");
        };
        witness.verify_signature()?;
        assert_eq!(&witness.issuer, target_key);
        assert_eq!(witness.payload.snapshot_id, snapshot.payload.snapshot_id);
        assert_eq!(witness.payload.request_hash, snapshot.payload.request_hash);
        assert_eq!(witness.payload.snapshot_hash, snapshot.hash()?);
        assert_eq!(witness.payload.final_page_hash, previous);
        assert_eq!(witness.payload.page_count, page_count);
        assert_eq!(witness.payload.entry_count, entry_count);
        assert!(page_count > 0 && entry_count > 0);
        assert_eq!(policy_counts(&fixture.store)?, before_b);
        assert!(
            fixture
                .store
                .authorization_state()
                .account(&fixture.executor.account)
                .is_none()
        );
        let target = node.store.lock().await;
        assert_eq!(policy_counts(&target)?, before_c);
        assert_eq!(
            target.connection.query_row(
                "SELECT count(*) FROM node_certificates WHERE node_id=?",
                [&b_node_id],
                |row| row.get::<_, u64>(0),
            )?,
            0
        );
        assert!(target.authorize_transport_node(&b_node_id, now()).is_err());
        drop(target);
        drop(reader);
        tasks.abort_all();
        while tasks.join_next().await.is_some() {}
        Ok(())
    }

    #[test]
    fn node_policy_reader_sessions_cannot_upgrade_to_other_methods() -> Result<()> {
        let mut session = Session {
            public_key: String::new(),
            node_policy_read_only: false,
            authorization: None,
            subscription: None,
            remote_subscription: None,
        };
        session.select_method("info")?;
        session.select_method(NODE_POLICY_READ_METHOD)?;
        session.select_method(NODE_POLICY_READ_METHOD)?;
        for method in [
            "info",
            "authenticate",
            "prepare_invocation",
            "rpc",
            "subscribe",
            "identity",
            "unknown",
        ] {
            assert!(session.select_method(method).is_err(), "{method}");
        }
        assert!(session.authorization.is_none());
        assert!(session.subscription.is_none());
        assert!(session.remote_subscription.is_none());
        Ok(())
    }

    #[test]
    fn raw_requests_reject_duplicate_keys_at_every_depth() -> Result<()> {
        let request = parse_request(
            br#" { "params": {"targets":[{"target":"a"},{"target":"b"}],"proof":{"issuer":"root","account":"owner"},"positive":18446744073709551615,"negative":-1,"fraction":1.5,"active":true,"empty":null}, "method":"rpc", "id":"read" } "#,
        )?;
        validate_request(&request)?;
        assert_eq!(request.id, "read");
        assert_eq!(request.method, "rpc");
        assert_eq!(
            request.params,
            json!({"targets":[{"target":"a"},{"target":"b"}],"proof":{"issuer":"root","account":"owner"},"positive":u64::MAX,"negative":-1,"fraction":1.5,"active":true,"empty":null})
        );
        for bytes in [
            br#"{"id":"read","method":"rpc","params":{},"params":{}}"#.as_slice(),
            br#"{"id":"read","method":"rpc","params":{"proof":{"issuer":"root","issuer":"root"}}}"#,
            br#"{"id":"read","method":"rpc","params":{"scope":{"account":"owner","account":"other"}}}"#,
            br#"{"id":"read","method":"rpc","params":{"bindings":[{"target":"a","target":"b"}]}}"#,
            br#"{"id":"read","method":"rpc","params":{"proof":{"issuer":"root","\u0069ssuer":"root"}}}"#,
        ] {
            let error = match parse_request(bytes) {
                Ok(_) => panic!("duplicate object key was accepted"),
                Err(error) => error,
            };
            assert!(error.to_string().contains("duplicate JSON object key"));
        }
        Ok(())
    }

    #[test]
    fn encrypted_requests_require_bounded_ids_and_reject_legacy_invocations() {
        let request: Request =
            serde_json::from_value(json!({"id":"read", "method":"rpc", "params":{}})).unwrap();
        assert!(validate_request(&request).is_ok());
        assert!(
            serde_json::from_value::<Request>(
                json!({"id":"read", "method":"rpc", "params":{}, "human":true})
            )
            .is_err()
        );
        let request = Request {
            id: "\n".into(),
            method: "rpc".into(),
            params: json!({}),
            invocation: None,
        };
        assert!(validate_request(&request).is_err());
    }
}
