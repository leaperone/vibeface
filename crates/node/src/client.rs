//! Noise proves device possession. Every business request carries an independent Invocation.

use crate::{
    crypto::LocalIdentity,
    network::{NetworkSocket, Service},
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
    ClientAuthorizationBundle, ClientScope, DeviceId, Invocation, PolicyPin, PublicKey,
    ResourceRecord, Signed, TransportEvidence, VerifiedClientAuthorization,
    verify_client_authorization,
};
use vibeface_secure::{Channel, Handshake};

const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
const CALL_TIMEOUT: Duration = Duration::from_secs(120);
const IDLE_TIMEOUT: Duration = Duration::from_secs(90);
const HEARTBEAT: Duration = Duration::from_secs(20);

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
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Authentication {
    scope: ClientScope,
    authorization: ClientAuthorizationBundle,
    pins: Vec<PolicyPin>,
    resources: Vec<Signed<ResourceRecord>>,
}
struct Session {
    public_key: String,
    authorization: Option<VerifiedClientAuthorization>,
    subscription: Option<Access>,
    remote_subscription: Option<RemoteSubscription>,
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
        mut socket: NetworkSocket,
        identity: &LocalIdentity,
        target: &str,
    ) -> Result<Self> {
        let channel = tokio::time::timeout(HANDSHAKE_TIMEOUT, async {
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
                            let result = dispatch(&node, &mut session, request).await;
                            let (id, result) = result;
                            let response = match result {
                                Ok(result) => Response { id, result, error: None },
                                Err(error) => Response { id, result: Value::Null, error: Some(error.to_string()) },
                            };
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
    match method {
        "info" => Ok(node.info()),
        "authenticate" => {
            let envelope = node
                .identity_command(&session.public_key, "authenticate", &params)
                .await?;
            let input: Authentication = serde_json::from_value(envelope)?;
            let pins = {
                let store = node.store.lock().await;
                serde_json::to_string(store.trusted_policy_pins())?
            };
            let verified = secure(verify_client_authorization(
                &serde_json::to_string(&input.authorization)?,
                &pins,
                input.scope.account_id.as_str(),
                input.scope.space_id.as_str(),
                &session.public_key,
                crate::runtime::now_ms() as f64,
            ))?;
            ensure!(
                verified.scope() == &input.scope,
                "client scope differs from verified proof"
            );
            ensure!(
                verified.resources() == input.resources,
                "resource directory differs from verified policy"
            );
            {
                let store = node.store.lock().await;
                for stamp in verified.heads() {
                    ensure!(
                        store.authorization_state().head(&stamp.domain) == Some(stamp),
                        "policy synchronization is required"
                    );
                }
            }
            session.subscription = None;
            session.remote_subscription = None;
            session.authorization = Some(verified);
            Ok(
                json!({"scope":input.scope,"authorization":input.authorization,"pins":input.pins,"resources":input.resources}),
            )
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
        assert!(serde_json::from_value::<Authentication>(json!({"network_id":"legacy"})).is_err());
        let request = Request {
            id: "\n".into(),
            method: "rpc".into(),
            params: json!({}),
            invocation: None,
        };
        assert!(validate_request(&request).is_err());
    }
}
