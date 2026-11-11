use crate::identity::{Entry, PROLOGUE};
use anyhow::{Context, Result, bail, ensure};
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use std::{collections::HashMap, sync::Arc, time::Duration};
use tokio::sync::{broadcast, mpsc, oneshot, watch};
use tokio_tungstenite::tungstenite::Message;
use vibeface_secure::{Channel, Handshake, Identity};
use vibeface_secure_ffi::lan::{LanStream, lan_connect};

const LIMIT: usize = 64;
const MAX_REQUEST: usize = 1024 * 1024;
const TIMEOUT: Duration = Duration::from_secs(60);
type Socket =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

#[derive(Clone, Debug)]
pub enum Endpoint {
    Gateway(Entry),
    Local(crate::LocalGrant),
    Preferred {
        gateway: Entry,
        addresses: Vec<String>,
    },
    Lan {
        node_id: String,
        addresses: Vec<String>,
    },
}
impl Endpoint {
    pub fn validate(&self) -> Result<()> {
        vibeface_secure::validate_device_id(self.node_id())?;
        let addresses = match self {
            Self::Gateway(entry) => {
                client_url(&entry.origin)?;
                None
            }
            Self::Local(grant) => {
                grant.websocket_request(grant.client_id())?;
                None
            }
            Self::Preferred { gateway, addresses } => {
                client_url(&gateway.origin)?;
                Some(addresses)
            }
            Self::Lan { addresses, .. } => {
                ensure!(!addresses.is_empty(), "direct Node entry has no addresses");
                Some(addresses)
            }
        };
        if let Some(addresses) = addresses {
            ensure!(
                addresses.len() <= vibeface_secure::link::MAX_ADDRS,
                "too many Node address hints"
            );
            for address in addresses {
                let socket: std::net::SocketAddr = address
                    .parse()
                    .context("direct hints must be numeric socket addresses")?;
                ensure!(
                    socket.port() != 0
                        && !socket.ip().is_unspecified()
                        && !socket.ip().is_multicast(),
                    "invalid direct Node address hint"
                );
            }
        }
        Ok(())
    }
    pub fn node_id(&self) -> &str {
        match self {
            Self::Gateway(e) => &e.node_id,
            Self::Preferred { gateway, .. } => &gateway.node_id,
            Self::Local(grant) => &grant.entry().node_id,
            Self::Lan { node_id, .. } => node_id,
        }
    }
}

/// An explicit Node refusal, distinct from a transport failure with an unknown outcome.
#[derive(Debug)]
pub struct Refused(pub String);
impl std::fmt::Display for Refused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}
impl std::error::Error for Refused {}

#[derive(Debug)]
pub(crate) struct Disconnected;
impl std::fmt::Display for Disconnected {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("connection closed")
    }
}
impl std::error::Error for Disconnected {}
pub(crate) fn transport_failure(error: &anyhow::Error) -> bool {
    if error.is::<Disconnected>() || error.is::<tokio::time::error::Elapsed>() {
        return true;
    }
    let io = if let Some(error) = error.downcast_ref::<tokio_tungstenite::tungstenite::Error>() {
        match error {
            tokio_tungstenite::tungstenite::Error::Io(io) => Some(io),
            tokio_tungstenite::tungstenite::Error::ConnectionClosed
            | tokio_tungstenite::tungstenite::Error::AlreadyClosed => return true,
            _ => None,
        }
    } else {
        error.downcast_ref::<std::io::Error>()
    };
    io.is_some_and(|error| {
        matches!(
            error.kind(),
            std::io::ErrorKind::ConnectionRefused
                | std::io::ErrorKind::ConnectionReset
                | std::io::ErrorKind::ConnectionAborted
                | std::io::ErrorKind::TimedOut
                | std::io::ErrorKind::NotConnected
                | std::io::ErrorKind::AddrNotAvailable
                | std::io::ErrorKind::NetworkUnreachable
                | std::io::ErrorKind::HostUnreachable
                | std::io::ErrorKind::BrokenPipe
                | std::io::ErrorKind::UnexpectedEof
        )
    })
}
pub(crate) type BeforeWrite =
    Box<dyn FnOnce() -> futures_util::future::BoxFuture<'static, Result<()>> + Send>;
struct Request {
    id: String,
    bytes: Vec<u8>,
    reply: oneshot::Sender<Result<Value>>,
    before_write: Option<BeforeWrite>,
}
struct Pending {
    reply: oneshot::Sender<Result<Value>>,
    deadline: tokio::time::Instant,
}

/// A bounded multiplexed Noise connection. Dropping a caller never cancels the stream reader.
#[derive(Clone)]
pub struct Connection {
    requests: mpsc::Sender<Request>,
    pushes: broadcast::Sender<Value>,
    shutdown: watch::Sender<bool>,
}
impl Connection {
    pub(crate) fn disconnected() -> Self {
        let (requests, receiver) = mpsc::channel(LIMIT);
        drop(receiver);
        let (pushes, _) = broadcast::channel(128);
        let (shutdown, _) = watch::channel(true);
        Self {
            requests,
            pushes,
            shutdown,
        }
    }
    pub async fn open(origin: &str, identity: &Identity, target: &str) -> Result<Self> {
        Self::open_endpoint(
            &Endpoint::Gateway(Entry {
                origin: origin.into(),
                node_id: target.into(),
            }),
            identity,
        )
        .await
    }
    pub async fn open_endpoint(endpoint: &Endpoint, identity: &Identity) -> Result<Self> {
        endpoint.validate()?;
        if let Endpoint::Preferred { gateway, addresses } = endpoint {
            if !addresses.is_empty() {
                let direct = Endpoint::Lan {
                    node_id: gateway.node_id.clone(),
                    addresses: addresses.clone(),
                };
                if let Ok(Ok(connection)) = tokio::time::timeout(
                    Duration::from_secs(3),
                    Self::open_transport(&direct, identity),
                )
                .await
                {
                    return Ok(connection);
                }
            }
            return Self::open_transport(&Endpoint::Gateway(gateway.clone()), identity).await;
        }
        Self::open_transport(endpoint, identity).await
    }
    async fn open_transport(endpoint: &Endpoint, identity: &Identity) -> Result<Self> {
        tokio::time::timeout(Duration::from_secs(20), Self::handshake(endpoint, identity))
            .await
            .context("client handshake timed out")?
    }
    async fn handshake(endpoint: &Endpoint, identity: &Identity) -> Result<Self> {
        let mut transport = match endpoint {
            Endpoint::Gateway(entry) => {
                let (mut socket, _) = tokio_tungstenite::connect_async_with_config(
                    client_url(&entry.origin)?,
                    Some(websocket_config()),
                    false,
                )
                .await?;
                socket
                    .send(Message::Text(
                        json!({"type":"connect","target":entry.node_id})
                            .to_string()
                            .into(),
                    ))
                    .await?;
                Transport::Web(Box::new(socket))
            }
            Endpoint::Local(grant) => {
                let (socket, _) = tokio_tungstenite::connect_async_with_config(
                    grant.websocket_request(&identity.device_id())?,
                    Some(websocket_config()),
                    false,
                )
                .await?;
                Transport::Web(Box::new(socket))
            }
            Endpoint::Preferred { .. } => {
                bail!("preferred route was not resolved before Noise handshake")
            }
            Endpoint::Lan { node_id, addresses } => {
                Transport::Lan(lan_connect(node_id.clone(), addresses.clone(), 10_000).await?)
            }
        };
        let mut handshake =
            Handshake::signed(true, identity, Some(endpoint.node_id().into()), PROLOGUE)?;
        transport.send(handshake.write(&[])?).await?;
        loop {
            match transport.recv().await? {
                Message::Binary(bytes) => {
                    ensure!(
                        handshake.read(&bytes)?.is_empty(),
                        "unexpected handshake payload"
                    );
                    break;
                }
                Message::Text(text) => ensure!(
                    serde_json::from_str::<Value>(&text)?["type"] == "queued",
                    "entry refused connection"
                ),
                Message::Close(_) => return Err(Disconnected.into()),
                _ => {}
            }
        }
        transport.send(handshake.write(&[])?).await?;
        ensure!(
            handshake.remote_id().as_deref() == Some(endpoint.node_id()),
            "Node device key mismatch"
        );
        Ok(Self::spawn(transport, handshake.finish()?))
    }
    fn spawn(transport: Transport, channel: Channel) -> Self {
        let (requests, incoming) = mpsc::channel(LIMIT);
        let (pushes, _) = broadcast::channel(128);
        let (shutdown, stop) = watch::channel(false);
        let (frames_tx, frames) = mpsc::channel(16);
        let (writer, reader) = transport.split();
        let reader_task = tokio::spawn(async move {
            reader.run(frames_tx).await;
        });
        let events = pushes.clone();
        tokio::spawn(async move {
            run(incoming, frames, writer, channel, events, stop).await;
            reader_task.abort();
        });
        Self {
            requests,
            pushes,
            shutdown,
        }
    }
    pub fn is_open(&self) -> bool {
        !*self.shutdown.borrow() && !self.requests.is_closed()
    }
    pub fn close(&self) {
        self.shutdown.send_replace(true);
    }
    pub fn subscribe(&self) -> broadcast::Receiver<Value> {
        self.pushes.subscribe()
    }
    pub async fn call(&self, method: &str, params: Value) -> Result<Value> {
        self.call_bytes(serde_json::to_vec(
            &json!({"id":uuid::Uuid::new_v4().to_string(),"method":method,"params":params}),
        )?)
        .await
    }
    /// Sends the original persisted envelope, without regenerating its ID or signature.
    pub async fn call_bytes(&self, bytes: Vec<u8>) -> Result<Value> {
        self.enqueue(bytes, None).await
    }
    pub(crate) async fn call_staged(
        &self,
        bytes: Vec<u8>,
        before_write: BeforeWrite,
    ) -> Result<Value> {
        self.enqueue(bytes, Some(before_write)).await
    }
    async fn enqueue(&self, bytes: Vec<u8>, before_write: Option<BeforeWrite>) -> Result<Value> {
        ensure!(!*self.shutdown.borrow(), "connection is closed");
        ensure!(bytes.len() <= MAX_REQUEST, "request exceeds limit");
        let value: Value = serde_json::from_slice(&bytes)?;
        let id = value["id"]
            .as_str()
            .context("request ID missing")?
            .to_owned();
        let (reply, response) = oneshot::channel();
        tokio::time::timeout(
            TIMEOUT,
            self.requests.send(Request {
                id,
                bytes,
                reply,
                before_write,
            }),
        )
        .await
        .context("request queue did not admit the operation")?
        .map_err(|_| anyhow::anyhow!("connection closed before request admission"))?;
        response
            .await
            .context("connection closed; outcome unknown")?
    }
}

async fn run(
    mut requests: mpsc::Receiver<Request>,
    mut frames: mpsc::Receiver<Result<Message>>,
    mut writer: Writer,
    mut channel: Channel,
    pushes: broadcast::Sender<Value>,
    mut shutdown: watch::Receiver<bool>,
) {
    let mut pending: HashMap<String, Pending> = HashMap::new();
    let mut tick = tokio::time::interval(Duration::from_millis(100));
    let result: Result<()> = async {
        loop {
            ensure!(!*shutdown.borrow(), "connection closed; outcome unknown");
            tokio::select! {
                _ = shutdown.changed() => { return Ok(()); }
                request = requests.recv(), if pending.len() < LIMIT => {
                    let Some(mut request) = request else { return Ok(()); };
                    if request.reply.is_closed() { continue; }
                    if pending.contains_key(&request.id) {
                        let _ = request.reply.send(Err(anyhow::anyhow!("request already in flight"))); continue;
                    }
                    if let Some(before_write) = request.before_write.take()
                        && let Err(error) = before_write().await
                    {
                        let _ = request.reply.send(Err(error));
                        continue;
                    }
                    ensure!(!*shutdown.borrow(),"connection closed before write");
                    let records = channel.seal_message(&request.bytes)?;
                    pending.insert(request.id, Pending { reply: request.reply, deadline: tokio::time::Instant::now() + TIMEOUT });
                    for record in records { tokio::time::timeout(Duration::from_secs(10), writer.send(record)).await.context("write timed out; outcome unknown")??; }
                }
                frame = frames.recv() => {
                    let frame = frame.ok_or(Disconnected)??;
                    match frame {
                        Message::Binary(bytes) => {
                            let Some(plain) = channel.open_record(&bytes)? else { continue; };
                            ensure!(plain.len() <= 4 * MAX_REQUEST, "response exceeds limit");
                            let mut value: Value = serde_json::from_slice(&plain)?;
                            if let Some(id) = value["id"].as_str() {
                                if let Some(request) = pending.remove(id) {
                                    let answer = match value["error"].as_str() { Some(error) => Err(Refused(error.into()).into()), None => Ok(value["result"].take()) };
                                    let _ = request.reply.send(answer);
                                }
                            } else { let _ = pushes.send(value); }
                        }
                        Message::Close(_) => return Err(Disconnected.into()),
                        _ => {}
                    }
                }
                _ = tick.tick() => {
                    let expired: Vec<_> = pending.iter().filter(|(_,p)| p.deadline <= tokio::time::Instant::now()).map(|(id,_)| id.clone()).collect();
                    for id in expired { if let Some(p) = pending.remove(&id) { let _ = p.reply.send(Err(anyhow::anyhow!("request timed out; outcome unknown"))); } }
                }
            }
        }
    }.await;
    let failure = result.err();
    let lost = failure.as_ref().is_none_or(transport_failure);
    let error = failure
        .map(|e| e.to_string())
        .unwrap_or_else(|| "connection closed".into());
    for (_, p) in pending {
        let _ = p.reply.send(Err(if lost {
            Disconnected.into()
        } else {
            anyhow::anyhow!(error.clone())
        }));
    }
    let _ = pushes.send(json!({"type":"disconnected"}));
    writer.close().await;
}

fn websocket_config() -> tokio_tungstenite::tungstenite::protocol::WebSocketConfig {
    tokio_tungstenite::tungstenite::protocol::WebSocketConfig::default()
        .max_message_size(Some(65535))
        .max_frame_size(Some(65535))
        .max_write_buffer_size(256 * 1024)
}
enum Transport {
    Web(Box<Socket>),
    Lan(Arc<LanStream>),
}
impl Transport {
    async fn send(&mut self, bytes: Vec<u8>) -> Result<()> {
        match self {
            Self::Web(s) => Ok(s.send(Message::Binary(bytes.into())).await?),
            Self::Lan(s) => Ok(s.send(bytes)?),
        }
    }
    async fn recv(&mut self) -> Result<Message> {
        match self {
            Self::Web(s) => s.next().await.ok_or(Disconnected)?.map_err(Into::into),
            Self::Lan(s) => lan_frame(s).await,
        }
    }
    fn split(self) -> (Writer, Reader) {
        match self {
            Self::Web(s) => {
                let (w, r) = (*s).split();
                (Writer::Web(w), Reader::Web(r))
            }
            Self::Lan(s) => (Writer::Lan(s.clone()), Reader::Lan(s)),
        }
    }
}
enum Writer {
    Web(futures_util::stream::SplitSink<Socket, Message>),
    Lan(Arc<LanStream>),
}
impl Writer {
    async fn send(&mut self, bytes: Vec<u8>) -> Result<()> {
        match self {
            Self::Web(s) => Ok(s.send(Message::Binary(bytes.into())).await?),
            Self::Lan(s) => send_lan(s, bytes).await,
        }
    }
    async fn close(&mut self) {
        match self {
            Self::Web(s) => {
                let _ = s.close().await;
            }
            Self::Lan(s) => s.close(),
        }
    }
}
enum Reader {
    Web(futures_util::stream::SplitStream<Socket>),
    Lan(Arc<LanStream>),
}
impl Reader {
    async fn run(mut self, tx: mpsc::Sender<Result<Message>>) {
        loop {
            let frame = match &mut self {
                Self::Web(s) => match s.next().await {
                    Some(f) => f.map_err(Into::into),
                    None => return,
                },
                Self::Lan(s) => lan_frame(s).await,
            };
            let ended = frame.is_err() || matches!(&frame, Ok(Message::Close(_)));
            if tx.send(frame).await.is_err() || ended {
                return;
            }
        }
    }
}
async fn send_lan(stream: &Arc<LanStream>, bytes: Vec<u8>) -> Result<()> {
    loop {
        match stream.send(bytes.clone()) {
            Ok(()) => return Ok(()),
            Err(vibeface_secure_ffi::SecureError::Failed { reason })
                if reason == "stream backpressure limit reached" =>
            {
                tokio::time::sleep(Duration::from_millis(5)).await
            }
            Err(error) => return Err(error.into()),
        }
    }
}
async fn lan_frame(stream: &Arc<LanStream>) -> Result<Message> {
    let frame = stream
        .clone()
        .recv()
        .await?
        .context("LAN connection closed")?;
    Ok(if frame.text {
        Message::Text(String::from_utf8(frame.data)?.into())
    } else {
        Message::Binary(frame.data.into())
    })
}
pub(crate) fn client_url(origin: &str) -> Result<String> {
    let mut url = reqwest::Url::parse(origin)?;
    ensure!(
        url.username().is_empty() && url.password().is_none() && url.fragment().is_none(),
        "invalid entry URL"
    );
    let scheme = match url.scheme() {
        "http" => "ws",
        "https" => "wss",
        _ => bail!("entry must use http or https"),
    };
    url.set_scheme(scheme)
        .map_err(|()| anyhow::anyhow!("invalid entry URL"))?;
    url.set_path("/api/client");
    url.set_query(None);
    Ok(url.to_string())
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    type ServerSocket = tokio_tungstenite::WebSocketStream<tokio::net::TcpStream>;
    pub(crate) async fn server(
        listener: tokio::net::TcpListener,
        identity: &Identity,
    ) -> Result<(ServerSocket, Channel)> {
        let (tcp, _) = listener.accept().await?;
        let mut socket = tokio_tungstenite::accept_async(tcp).await?;
        let connect = socket.next().await.context("connect frame")??;
        ensure!(matches!(connect, Message::Text(_)), "connect frame missing");
        let mut handshake = Handshake::signed(false, identity, None, PROLOGUE)?;
        let Message::Binary(first) = socket.next().await.context("first handshake")?? else {
            bail!("expected Noise")
        };
        handshake.read(&first)?;
        socket
            .send(Message::Binary(handshake.write(&[])?.into()))
            .await?;
        let Message::Binary(last) = socket.next().await.context("last handshake")?? else {
            bail!("expected Noise")
        };
        handshake.read(&last)?;
        Ok((socket, handshake.finish()?))
    }
    pub(crate) async fn receive(socket: &mut ServerSocket, channel: &mut Channel) -> Result<Value> {
        loop {
            let Message::Binary(bytes) = socket.next().await.context("frame")?? else {
                continue;
            };
            if let Some(bytes) = channel.open_record(&bytes)? {
                return Ok(serde_json::from_slice(&bytes)?);
            }
        }
    }
    pub(crate) async fn send(
        socket: &mut ServerSocket,
        channel: &mut Channel,
        value: Value,
    ) -> Result<()> {
        for bytes in channel.seal_message(&serde_json::to_vec(&value)?)? {
            socket.send(Message::Binary(bytes.into())).await?;
        }
        Ok(())
    }
    #[tokio::test]
    async fn noise_multiplexes_out_of_order_responses_and_keeps_pushes() -> Result<()> {
        let node = Identity::generate()?;
        let client = Identity::generate()?;
        let target = node.device_id();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let origin = format!("http://{}", listener.local_addr()?);
        let node_task = tokio::spawn(async move {
            let (mut socket, mut channel) = server(listener, &node).await?;
            let one = receive(&mut socket, &mut channel).await?;
            let two = receive(&mut socket, &mut channel).await?;
            send(
                &mut socket,
                &mut channel,
                json!({"type":"changed","scope":"chat"}),
            )
            .await?;
            send(
                &mut socket,
                &mut channel,
                json!({"id":two["id"],"result":two["params"]}),
            )
            .await?;
            send(
                &mut socket,
                &mut channel,
                json!({"id":one["id"],"result":one["params"]}),
            )
            .await?;
            Ok::<_, anyhow::Error>(())
        });
        let connection = Connection::open(&origin, &client, &target).await?;
        let mut events = connection.subscribe();
        let (one, two) = tokio::join!(
            connection.call("one", json!({"answer":1})),
            connection.call("two", json!({"answer":2}))
        );
        assert_eq!(one?, json!({"answer":1}));
        assert_eq!(two?, json!({"answer":2}));
        assert_eq!(
            events.recv().await?,
            json!({"type":"changed","scope":"chat"})
        );
        node_task.await??;
        Ok(())
    }
    #[tokio::test]
    async fn cancelling_one_request_does_not_corrupt_the_next_noise_record() -> Result<()> {
        let node = Identity::generate()?;
        let client = Identity::generate()?;
        let target = node.device_id();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let origin = format!("http://{}", listener.local_addr()?);
        let (seen_tx, seen_rx) = oneshot::channel();
        let (continue_tx, continue_rx) = oneshot::channel();
        let server_task = tokio::spawn(async move {
            let (mut socket, mut channel) = server(listener, &node).await?;
            let first = receive(&mut socket, &mut channel).await?;
            let _ = seen_tx.send(());
            continue_rx.await?;
            send(
                &mut socket,
                &mut channel,
                json!({"id":first["id"],"result":{"cancelled_caller":true}}),
            )
            .await?;
            let second = receive(&mut socket, &mut channel).await?;
            send(
                &mut socket,
                &mut channel,
                json!({"id":second["id"],"result":{"alive":true}}),
            )
            .await?;
            Ok::<_, anyhow::Error>(())
        });
        let connection = Connection::open(&origin, &client, &target).await?;
        let first = connection.clone();
        let caller = tokio::spawn(async move { first.call("one", json!({})).await });
        seen_rx.await?;
        caller.abort();
        let _ = continue_tx.send(());
        assert_eq!(
            connection.call("two", json!({})).await?,
            json!({"alive":true})
        );
        server_task.await??;
        Ok(())
    }
    #[tokio::test]
    async fn wrong_noise_device_key_is_rejected() -> Result<()> {
        let node = Identity::generate()?;
        let client = Identity::generate()?;
        let wrong = Identity::generate()?.device_id();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let origin = format!("http://{}", listener.local_addr()?);
        let server_task = tokio::spawn(async move { server(listener, &node).await });
        assert!(Connection::open(&origin, &client, &wrong).await.is_err());
        server_task.abort();
        Ok(())
    }
}

#[cfg(test)]
mod admission_tests {
    use super::tests::{receive, send, server};
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[tokio::test]
    async fn cancellation_while_the_bounded_queue_is_full_does_not_start_a_write() -> Result<()> {
        let (requests, mut queue) = mpsc::channel(1);
        let (pushes, _) = broadcast::channel(1);
        let (shutdown, _) = watch::channel(false);
        let connection = Connection {
            requests: requests.clone(),
            pushes,
            shutdown,
        };
        let (reply, _) = oneshot::channel();
        requests
            .send(Request {
                id: "occupied".into(),
                bytes: vec![],
                reply,
                before_write: None,
            })
            .await
            .map_err(|_| anyhow::anyhow!("fixture queue closed"))?;
        let starts = Arc::new(AtomicUsize::new(0));
        let observed = starts.clone();
        let (ready, waiting) = oneshot::channel();
        let task = tokio::spawn(async move {
            let _ = ready.send(());
            connection
                .call_staged(
                    serde_json::to_vec(&json!({"id":"not-admitted","method":"rpc","params":{}}))?,
                    Box::new(move || {
                        Box::pin(async move {
                            observed.fetch_add(1, Ordering::SeqCst);
                            Ok(())
                        })
                    }),
                )
                .await
        });
        waiting.await?;
        tokio::task::yield_now().await;
        task.abort();
        let _ = task.await;
        assert_eq!(
            queue.recv().await.context("occupied queue entry")?.id,
            "occupied"
        );
        assert!(
            queue.try_recv().is_err(),
            "cancelled unsent call never entered the queue"
        );
        assert_eq!(starts.load(Ordering::SeqCst), 0);
        Ok(())
    }
    #[tokio::test]
    async fn refused_before_write_keeps_the_noise_stream_usable_without_sending_the_operation()
    -> Result<()> {
        let node = Identity::generate()?;
        let client = Identity::generate()?;
        let target = node.device_id();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let origin = format!("http://{}", listener.local_addr()?);
        let task = tokio::spawn(async move {
            let (mut socket, mut channel) = server(listener, &node).await?;
            let request = receive(&mut socket, &mut channel).await?;
            assert_eq!(request["method"], "read-after-blocked-write");
            send(
                &mut socket,
                &mut channel,
                json!({"id":request["id"],"result":{"read":true}}),
            )
            .await?;
            Ok::<_, anyhow::Error>(())
        });
        let connection = Connection::open(&origin, &client, &target).await?;
        let denied = connection
            .call_staged(
                serde_json::to_vec(&json!({"id":"blocked","method":"must-not-send","params":{}}))?,
                Box::new(|| Box::pin(async { anyhow::bail!("earlier outcome is unknown") })),
            )
            .await;
        assert!(
            denied
                .unwrap_err()
                .to_string()
                .contains("earlier outcome is unknown")
        );
        assert_eq!(
            connection
                .call("read-after-blocked-write", json!({}))
                .await?,
            json!({"read":true})
        );
        task.await??;
        Ok(())
    }
}

#[cfg(test)]
mod preferred_tests {
    use super::tests::{receive, send, server};
    use super::*;
    #[tokio::test]
    async fn preferred_route_attempts_numeric_direct_transport_then_pins_the_same_gateway_node()
    -> Result<()> {
        let node = Identity::generate()?;
        let client = Identity::generate()?;
        let node_id = node.device_id();
        let silent = tokio::net::UdpSocket::bind("127.0.0.1:0").await?;
        let address = silent.local_addr()?.to_string();
        let direct = tokio::spawn(async move {
            let mut packet = [0u8; 2048];
            let (count, _) =
                tokio::time::timeout(Duration::from_secs(5), silent.recv_from(&mut packet))
                    .await??;
            Ok::<_, anyhow::Error>((count, tokio::time::Instant::now(), silent))
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let gateway = Entry {
            origin: format!("http://{}", listener.local_addr()?),
            node_id: node_id.clone(),
        };
        let task = tokio::spawn(async move {
            let (mut socket, mut channel) = server(listener, &node).await?;
            let connected_at = tokio::time::Instant::now();
            let request = receive(&mut socket, &mut channel).await?;
            assert_eq!(request["method"], "read");
            send(
                &mut socket,
                &mut channel,
                json!({"id":request["id"],"result":{"node_id":node.device_id()}}),
            )
            .await?;
            Ok::<_, anyhow::Error>(connected_at)
        });
        let endpoint = Endpoint::Preferred {
            gateway,
            addresses: vec![address],
        };
        let connection = Connection::open_endpoint(&endpoint, &client).await?;
        assert_eq!(
            connection.call("read", json!({})).await?["node_id"],
            node_id
        );
        let (count, direct_at, _silent) = direct.await??;
        let gateway_at = task.await??;
        assert!(count > 0);
        assert!(
            direct_at < gateway_at,
            "the direct route was attempted before Gateway success"
        );
        Ok(())
    }
}
