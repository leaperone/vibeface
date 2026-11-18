//! Native outbound LAN transport used by the Rust client and the mobile FFI adapter.

use std::{fmt, sync::Arc, time::Duration};

use iroh::{
    Endpoint, EndpointAddr, EndpointId, RelayMode, TransportAddr,
    endpoint::{BindError, Connection, RecvStream, SendStream, presets},
};
use tokio::sync::{Mutex, OwnedSemaphorePermit, Semaphore, mpsc};
use vibeface_protocol::client_frame::{
    ALPN, FrameKind, HEADER_BYTES, decode_header, encode_header,
};
use vibeface_secure as secure;

pub use vibeface_protocol::client_frame::MAX_PAYLOAD_BYTES as MAX_FRAME_BYTES;

const MAX_MESSAGE_RECORDS: usize =
    (secure::MAX_APPLICATION_MESSAGE + 4).div_ceil(secure::RECORD_PLAINTEXT);
const MAX_QUEUED_FRAMES: usize = MAX_MESSAGE_RECORDS + 1;
const MAX_QUEUED_BYTES: usize = secure::MAX_APPLICATION_MESSAGE
    + 4
    + MAX_MESSAGE_RECORDS * (16 + HEADER_BYTES)
    + MAX_FRAME_BYTES
    + HEADER_BYTES;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LanError {
    Backpressure,
    Closed,
    FrameTooLarge,
    UnknownFrame,
    NoAddress,
    TimedOut,
    InvalidDeviceId(String),
    Transport(String),
}

impl fmt::Display for LanError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Backpressure => f.write_str("stream backpressure limit reached"),
            Self::Closed => f.write_str("stream closed"),
            Self::FrameTooLarge => f.write_str("frame too large"),
            Self::UnknownFrame => f.write_str("unknown frame"),
            Self::NoAddress => f.write_str("no LAN address"),
            Self::TimedOut => f.write_str("LAN connection timed out"),
            Self::InvalidDeviceId(error) | Self::Transport(error) => f.write_str(error),
        }
    }
}
impl std::error::Error for LanError {}

#[derive(Clone)]
pub struct LanEndpoint {
    endpoint: Endpoint,
}

impl LanEndpoint {
    pub async fn bind() -> Result<Self, BindError> {
        Ok(Self {
            endpoint: Endpoint::builder(presets::Minimal)
                .relay_mode(RelayMode::Disabled)
                .portmapper_config(iroh::endpoint::PortmapperConfig::Disabled)
                .bind()
                .await?,
        })
    }

    #[cfg(test)]
    fn from_endpoint(endpoint: Endpoint) -> Self {
        Self { endpoint }
    }

    pub async fn connect(
        &self,
        device_id: &str,
        addrs: &[String],
        timeout: Duration,
    ) -> Result<Arc<LanStream>, LanError> {
        secure::validate_device_id(device_id)
            .map_err(|error| LanError::InvalidDeviceId(error.to_string()))?;
        let id = device_id
            .parse::<EndpointId>()
            .map_err(|error| LanError::InvalidDeviceId(error.to_string()))?;
        let addrs: Vec<TransportAddr> = addrs
            .iter()
            .filter_map(|addr| addr.parse().ok())
            .map(TransportAddr::Ip)
            .collect();
        if addrs.is_empty() {
            return Err(LanError::NoAddress);
        }
        let endpoint = self.endpoint.clone();
        let owner = self.clone();
        let dial = async move {
            let connection = endpoint
                .connect(EndpointAddr::from_parts(id, addrs), ALPN)
                .await
                .map_err(|error| LanError::Transport(error.to_string()))?;
            let (send, recv) = connection
                .open_bi()
                .await
                .map_err(|error| LanError::Transport(error.to_string()))?;
            Ok(LanStream::new(owner, connection, send, recv))
        };
        tokio::time::timeout(timeout, dial)
            .await
            .map_err(|_| LanError::TimedOut)?
    }
}

pub async fn lan_connect(
    device_id: &str,
    addrs: &[String],
    timeout: Duration,
) -> Result<Arc<LanStream>, LanError> {
    LanEndpoint::bind()
        .await
        .map_err(|error| LanError::Transport(error.to_string()))?
        .connect(device_id, addrs, timeout)
        .await
}

fn frame(header: [u8; HEADER_BYTES], data: &[u8]) -> Vec<u8> {
    let mut frame = Vec::with_capacity(HEADER_BYTES + data.len());
    frame.extend_from_slice(&header);
    frame.extend_from_slice(data);
    frame
}

struct QueuedFrame {
    header: [u8; HEADER_BYTES],
    data: Vec<u8>,
    _bytes: OwnedSemaphorePermit,
}
struct Outgoing {
    queue: mpsc::Sender<QueuedFrame>,
    bytes: Arc<Semaphore>,
}
impl Outgoing {
    fn new() -> (Self, mpsc::Receiver<QueuedFrame>) {
        let (queue, receiver) = mpsc::channel(MAX_QUEUED_FRAMES);
        (
            Self {
                queue,
                bytes: Arc::new(Semaphore::new(MAX_QUEUED_BYTES)),
            },
            receiver,
        )
    }
    fn send(&self, kind: FrameKind, data: Vec<u8>) -> Result<(), LanError> {
        let header = encode_header(kind, data.len()).map_err(|_| LanError::FrameTooLarge)?;
        let bytes = self
            .bytes
            .clone()
            .try_acquire_many_owned((data.len() + HEADER_BYTES) as u32)
            .map_err(|_| LanError::Backpressure)?;
        self.queue
            .try_send(QueuedFrame {
                header,
                data,
                _bytes: bytes,
            })
            .map_err(|error| match error {
                mpsc::error::TrySendError::Full(_) => LanError::Backpressure,
                mpsc::error::TrySendError::Closed(_) => LanError::Closed,
            })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LanFrame {
    pub text: bool,
    pub data: Vec<u8>,
}

struct ParsedFrame {
    kind: FrameKind,
    data: Vec<u8>,
}
struct FrameReader {
    recv: RecvStream,
    header: [u8; HEADER_BYTES],
    header_read: usize,
    payload: Vec<u8>,
    payload_read: usize,
    payload_len: Option<usize>,
}
impl FrameReader {
    fn new(recv: RecvStream) -> Self {
        Self {
            recv,
            header: [0; HEADER_BYTES],
            header_read: 0,
            payload: Vec::new(),
            payload_read: 0,
            payload_len: None,
        }
    }
    async fn read_frame(&mut self) -> Result<Option<ParsedFrame>, LanError> {
        while self.header_read < HEADER_BYTES {
            match self.recv.read(&mut self.header[self.header_read..]).await {
                Ok(Some(0)) | Ok(None) => {
                    return if self.header_read == 0 {
                        Ok(None)
                    } else {
                        Err(LanError::Transport(format!(
                            "stream finished early ({} bytes read)",
                            self.header_read
                        )))
                    };
                }
                Ok(Some(read)) => self.header_read += read,
                Err(error) => return Err(LanError::Transport(error.to_string())),
            }
        }
        if self.payload_len.is_none() {
            let header = decode_header(self.header).map_err(|_| LanError::FrameTooLarge)?;
            self.payload = vec![0; header.payload_len()];
            self.payload_len = Some(header.payload_len());
        }
        while self.payload_read < self.payload.len() {
            match self.recv.read(&mut self.payload[self.payload_read..]).await {
                Ok(Some(0)) | Ok(None) => {
                    return Err(LanError::Transport(format!(
                        "stream finished early ({} bytes read)",
                        self.payload_read
                    )));
                }
                Ok(Some(read)) => self.payload_read += read,
                Err(error) => return Err(LanError::Transport(error.to_string())),
            }
        }
        // Resolve the tag only after consuming its payload. This keeps a complete unknown frame
        // recoverable so the next frame can still be read.
        let kind = decode_header(self.header)
            .map_err(|_| LanError::FrameTooLarge)?
            .kind();
        let data = std::mem::take(&mut self.payload);
        self.header_read = 0;
        self.payload_read = 0;
        self.payload_len = None;
        Ok(Some(ParsedFrame {
            kind: kind.map_err(|_| LanError::UnknownFrame)?,
            data,
        }))
    }
}

pub struct LanStream {
    _endpoint: LanEndpoint,
    connection: Connection,
    outgoing: Outgoing,
    incoming: Mutex<FrameReader>,
}
impl LanStream {
    fn new(
        endpoint: LanEndpoint,
        connection: Connection,
        mut send: SendStream,
        recv: RecvStream,
    ) -> Arc<Self> {
        let (outgoing, mut queue) = Outgoing::new();
        let writer = connection.clone();
        tokio::spawn(async move {
            loop {
                let next = tokio::select! {
                    next = queue.recv() => next,
                    _ = writer.closed() => return,
                };
                let Some(next) = next else { break };
                if send
                    .write_all(&frame(next.header, &next.data))
                    .await
                    .is_err()
                {
                    writer.close(1u32.into(), b"write failed");
                    return;
                }
            }
            let _ = send.finish();
        });
        Arc::new(Self {
            _endpoint: endpoint,
            connection,
            outgoing,
            incoming: Mutex::new(FrameReader::new(recv)),
        })
    }
    pub fn send(&self, data: Vec<u8>) -> Result<(), LanError> {
        self.outgoing.send(FrameKind::Binary, data)
    }
    pub async fn recv(&self) -> Result<Option<LanFrame>, LanError> {
        let mut incoming = self.incoming.lock().await;
        loop {
            let Some(frame) = incoming.read_frame().await? else {
                return Ok(None);
            };
            match frame.kind {
                FrameKind::Binary => {
                    return Ok(Some(LanFrame {
                        text: false,
                        data: frame.data,
                    }));
                }
                FrameKind::Text => {
                    return Ok(Some(LanFrame {
                        text: true,
                        data: frame.data,
                    }));
                }
                FrameKind::Ping => {
                    let _ = self.outgoing.send(FrameKind::Pong, frame.data);
                }
                FrameKind::Pong => {}
            }
        }
    }
    pub fn close(&self) {
        self.connection.close(0u32.into(), b"closed");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use iroh::{
        SecretKey,
        endpoint::{RecvStream, SendStream},
    };
    use std::net::Ipv4Addr;

    const CONTEXT: &[u8] = b"vibeface:client:v1";
    #[test]
    fn outgoing_queue_holds_a_maximum_message_without_a_writer() {
        let (outgoing, mut queue) = Outgoing::new();
        outgoing
            .send(FrameKind::Pong, vec![0; MAX_FRAME_BYTES])
            .unwrap();
        let message_bytes = secure::MAX_APPLICATION_MESSAGE + 4;
        let full_records = message_bytes / secure::RECORD_PLAINTEXT;
        let remainder = message_bytes % secure::RECORD_PLAINTEXT;
        for _ in 0..full_records {
            outgoing
                .send(FrameKind::Binary, vec![0; secure::RECORD_PLAINTEXT + 16])
                .unwrap();
        }
        outgoing
            .send(FrameKind::Binary, vec![0; remainder + 16])
            .unwrap();
        assert_eq!(outgoing.bytes.available_permits(), 0);
        assert!(matches!(
            outgoing.send(FrameKind::Binary, vec![]),
            Err(LanError::Backpressure)
        ));
        while queue.try_recv().is_ok() {}
        assert_eq!(outgoing.bytes.available_permits(), MAX_QUEUED_BYTES);
    }

    #[test]
    fn outgoing_queue_bounds_bytes_and_releases_dropped_frames() {
        let (outgoing, mut queue) = Outgoing::new();
        assert!(matches!(
            outgoing.send(FrameKind::Binary, vec![0; MAX_FRAME_BYTES + 1]),
            Err(LanError::FrameTooLarge)
        ));
        let frame_size = MAX_FRAME_BYTES + HEADER_BYTES;
        for _ in 0..MAX_QUEUED_BYTES / frame_size {
            outgoing
                .send(FrameKind::Binary, vec![0; MAX_FRAME_BYTES])
                .unwrap();
        }
        assert!(matches!(
            outgoing.send(FrameKind::Binary, vec![0; MAX_FRAME_BYTES]),
            Err(LanError::Backpressure)
        ));
        drop(queue.try_recv().unwrap());
        outgoing
            .send(FrameKind::Binary, vec![0; MAX_FRAME_BYTES])
            .unwrap();
        drop(queue);
        assert_eq!(outgoing.bytes.available_permits(), MAX_QUEUED_BYTES);
    }

    async fn loopback_endpoints() -> (Endpoint, Endpoint) {
        let server = Endpoint::builder(presets::Minimal)
            .alpns(vec![ALPN.to_vec()])
            .relay_mode(RelayMode::Disabled)
            .clear_ip_transports()
            .bind_addr((std::net::Ipv4Addr::LOCALHOST, 0))
            .unwrap()
            .bind()
            .await
            .unwrap();
        let client = Endpoint::builder(presets::Minimal)
            .relay_mode(RelayMode::Disabled)
            .clear_ip_transports()
            .bind_addr((std::net::Ipv4Addr::LOCALHOST, 0))
            .unwrap()
            .bind()
            .await
            .unwrap();
        (server, client)
    }

    async fn loopback_node(secret_key: SecretKey) -> Endpoint {
        Endpoint::builder(presets::Minimal)
            .secret_key(secret_key)
            .alpns(vec![ALPN.to_vec()])
            .relay_mode(RelayMode::Disabled)
            .clear_ip_transports()
            .bind_addr((Ipv4Addr::LOCALHOST, 0))
            .unwrap()
            .bind()
            .await
            .unwrap()
    }

    async fn write_binary(send: &mut SendStream, data: &[u8]) {
        let header = encode_header(FrameKind::Binary, data.len()).unwrap();
        send.write_all(&frame(header, data)).await.unwrap();
    }

    async fn read_wire_frame(recv: &mut RecvStream) -> (FrameKind, Vec<u8>) {
        let mut raw = [0; HEADER_BYTES];
        recv.read_exact(&mut raw).await.unwrap();
        let header = decode_header(raw).unwrap();
        let mut data = vec![0; header.payload_len()];
        recv.read_exact(&mut data).await.unwrap();
        (header.kind().unwrap(), data)
    }

    fn node_id(seed: &[u8; 32]) -> String {
        hex::encode(SecretKey::from_bytes(seed).public().as_bytes())
    }

    #[tokio::test]
    async fn a_noise_session_runs_over_a_direct_stream() {
        tokio::time::timeout(Duration::from_secs(20), async {
            let node = secure::Identity::generate().unwrap();
            let seed: [u8; 32] = hex::decode(node.secret_hex()).unwrap().try_into().unwrap();
            let server = loopback_node(SecretKey::from_bytes(&seed)).await;
            let address = server.bound_sockets()[0].to_string();
            let serving = tokio::spawn(async move {
                let connection = loop {
                    let incoming = server.accept().await.unwrap();
                    if let Ok(connection) = incoming.await {
                        break connection;
                    }
                };
                let (mut send, mut recv) = connection.accept_bi().await.unwrap();
                let mut handshake = secure::Handshake::signed(false, &node, None, CONTEXT).unwrap();
                let (kind, first) = read_wire_frame(&mut recv).await;
                assert_eq!(kind, FrameKind::Binary);
                handshake.read(&first).unwrap();
                write_binary(&mut send, &handshake.write(&[]).unwrap()).await;
                let (kind, last) = read_wire_frame(&mut recv).await;
                assert_eq!(kind, FrameKind::Binary);
                handshake.read(&last).unwrap();
                let mut channel = handshake.finish().unwrap();
                send.write_all(&[2, 0, 0, 0, 0]).await.unwrap();
                let (kind, pong) = read_wire_frame(&mut recv).await;
                assert_eq!(kind, FrameKind::Pong);
                assert!(pong.is_empty());
                for record in channel.seal_message(b"reply").unwrap() {
                    write_binary(&mut send, &record).await;
                }
                send.write_all(&[
                    3, 0, 0, 0, 1, 0xff, 1, 0, 0, 0, 1, 0xff, 4, 0, 0, 0, 1, 0xff, 0, 0, 0, 0, 0,
                ])
                .await
                .unwrap();
                send.finish().unwrap();
                connection.closed().await;
            });

            let phone = secure::Identity::generate().unwrap();
            let unreachable = lan_connect(
                phone.device_id().as_str(),
                std::slice::from_ref(&address),
                Duration::from_secs(2),
            )
            .await;
            assert!(unreachable.is_err(), "another device key must not answer");
            let target = node_id(&seed);
            let stream = lan_connect(&target, &[address], Duration::from_secs(5))
                .await
                .unwrap();
            let mut client =
                secure::Handshake::signed(true, &phone, Some(target.clone()), CONTEXT).unwrap();
            client
                .set_proof(
                    phone.device_id().as_str(),
                    &phone.sign_message(&client.signing_bytes()),
                )
                .unwrap();
            stream.send(client.write(&[]).unwrap()).unwrap();
            let reply = stream.recv().await.unwrap().unwrap();
            assert!(!reply.text);
            client.read(&reply.data).unwrap();
            stream.send(client.write(&[]).unwrap()).unwrap();
            let mut channel = client.finish().unwrap();
            let record = stream.recv().await.unwrap().unwrap();
            assert_eq!(
                channel.open_record(&record.data).unwrap(),
                Some(b"reply".to_vec())
            );
            let text = stream.recv().await.unwrap().unwrap();
            assert!(text.text);
            assert_eq!(text.data, [0xff]);
            assert!(matches!(stream.recv().await, Err(LanError::UnknownFrame)));
            let empty = stream.recv().await.unwrap().unwrap();
            assert!(!empty.text);
            assert!(empty.data.is_empty());
            assert!(stream.recv().await.unwrap().is_none());
            stream.close();
            serving.await.unwrap();
        })
        .await
        .expect("direct stream fixture timed out");
    }

    #[tokio::test]
    async fn read_frame_rejects_partial_and_overlarge_frames() {
        let (server, client) = loopback_endpoints().await;
        let address =
            EndpointAddr::from_parts(server.id(), [TransportAddr::Ip(server.bound_sockets()[0])]);
        let (dialed, accepted) = tokio::join!(client.connect(address, ALPN), async {
            server.accept().await.unwrap().await
        },);
        let dialed = dialed.unwrap();
        let accepted = accepted.unwrap();
        for (wire, expected) in [
            (&[0, 0][..], "stream finished early (2 bytes read)"),
            (
                &[4, 0, 0, 0, 2, 0xff][..],
                "stream finished early (1 bytes read)",
            ),
            (&[4, 0, 16, 0, 1][..], "frame too large"),
        ] {
            let (mut send, _) = dialed.open_bi().await.unwrap();
            send.write_all(wire).await.unwrap();
            send.finish().unwrap();
            let (_, recv) = accepted.accept_bi().await.unwrap();
            let mut reader = FrameReader::new(recv);
            let result = reader.read_frame().await;
            if expected == "frame too large" {
                assert!(matches!(result, Err(LanError::FrameTooLarge)));
            } else {
                assert!(matches!(result, Err(LanError::Transport(reason)) if reason == expected));
            }
        }
    }

    #[tokio::test]
    async fn cancelling_a_partial_frame_preserves_the_next_read() {
        let (server, client) = loopback_endpoints().await;
        let address =
            EndpointAddr::from_parts(server.id(), [TransportAddr::Ip(server.bound_sockets()[0])]);
        let (dialed, accepted) = tokio::join!(client.connect(address, ALPN), async {
            server.accept().await.unwrap().await
        },);
        let dialed = dialed.unwrap();
        let accepted = accepted.unwrap();
        let (mut client_send, client_recv) = dialed.open_bi().await.unwrap();
        client_send.write_all(&[0, 0, 0, 0, 0]).await.unwrap();
        let (mut server_send, _) = accepted.accept_bi().await.unwrap();
        let stream = LanStream::new(
            LanEndpoint::from_endpoint(client),
            dialed,
            client_send,
            client_recv,
        );
        server_send.write_all(&[0, 0, 0, 0, 2, 0xaa]).await.unwrap();
        let pending = tokio::spawn({
            let stream = stream.clone();
            async move { stream.recv().await }
        });
        tokio::time::timeout(Duration::from_secs(2), async {
            tokio::time::sleep(Duration::from_millis(20)).await;
            assert!(
                !pending.is_finished(),
                "partial frame unexpectedly completed"
            );
        })
        .await
        .expect("partial frame guard timed out");
        pending.abort();
        server_send.write_all(&[0xbb, 0, 0, 0, 0, 0]).await.unwrap();
        server_send.finish().unwrap();
        let frame = stream.recv().await.unwrap().unwrap();
        assert_eq!(frame.data, [0xaa, 0xbb]);
        assert!(stream.recv().await.unwrap().unwrap().data.is_empty());
    }
}
