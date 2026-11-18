//! The app's direct path to a Node on the same network: one `vibeface/client/1` stream over iroh
//! QUIC carrying the frames a Gateway forwards, `tag ‖ u32 BE length ‖ bytes`. The QUIC key here
//! is new each run and proves nothing; Noise inside the stream proves both devices, as through a
//! Gateway.

use std::{
    future::Future,
    sync::{Arc, LazyLock},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use iroh::{
    Endpoint, EndpointAddr, EndpointId, RelayMode, TransportAddr,
    endpoint::{Connection, PortmapperConfig, ReadExactError, RecvStream, SendStream, presets},
};
use tokio::{
    runtime::Runtime,
    sync::{Mutex, OnceCell, OwnedSemaphorePermit, Semaphore, mpsc},
};
pub use vibeface_protocol::client_frame::MAX_PAYLOAD_BYTES as MAX_FRAME_BYTES;
use vibeface_protocol::client_frame::{
    ALPN, FrameKind, HEADER_BYTES, decode_header, encode_header,
};
use vibeface_secure as secure;

use crate::{Result, SecureError};

const MAX_MESSAGE_RECORDS: usize =
    (secure::MAX_APPLICATION_MESSAGE + 4).div_ceil(secure::RECORD_PLAINTEXT);
const MAX_QUEUED_FRAMES: usize = MAX_MESSAGE_RECORDS + 1;
// One complete application message, including Noise tags and frame headers, plus one control frame.
const MAX_QUEUED_BYTES: usize = secure::MAX_APPLICATION_MESSAGE
    + 4
    + MAX_MESSAGE_RECORDS * (16 + HEADER_BYTES)
    + MAX_FRAME_BYTES
    + HEADER_BYTES;

/// iroh runs on tokio, while uniffi polls exported futures from the app's thread; each call runs here.
static RUNTIME: LazyLock<Runtime> = LazyLock::new(|| {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .thread_name("vibeface-lan")
        .enable_all()
        .build()
        .expect("LAN runtime")
});
/// Made on first dial, on wildcard sockets with free ports. It accepts nothing: no ALPN is offered.
static ENDPOINT: OnceCell<Endpoint> = OnceCell::const_new();

fn failed(reason: impl std::fmt::Display) -> SecureError {
    SecureError::Failed {
        reason: reason.to_string(),
    }
}

async fn on_runtime<T: Send + 'static>(
    work: impl Future<Output = Result<T>> + Send + 'static,
) -> Result<T> {
    RUNTIME.spawn(work).await.map_err(failed)?
}

async fn endpoint() -> Result<&'static Endpoint> {
    ENDPOINT
        .get_or_try_init(|| async {
            Endpoint::builder(presets::Minimal)
                .relay_mode(RelayMode::Disabled)
                .portmapper_config(PortmapperConfig::Disabled)
                .bind()
                .await
                .map_err(failed)
        })
        .await
}

fn frame(header: [u8; HEADER_BYTES], data: &[u8]) -> Vec<u8> {
    let mut frame = Vec::with_capacity(HEADER_BYTES + data.len());
    frame.extend_from_slice(&header);
    frame.extend_from_slice(data);
    frame
}

/// `None` once the Node finished the stream between frames.
async fn read_frame(recv: &mut RecvStream) -> Result<Option<(FrameKind, Vec<u8>)>> {
    let mut header = [0; HEADER_BYTES];
    match recv.read_exact(&mut header).await {
        Ok(()) => {}
        Err(ReadExactError::FinishedEarly(0)) => return Ok(None),
        Err(error) => return Err(failed(error)),
    }
    let header = decode_header(header).map_err(|_| failed("frame too large"))?;
    let mut data = vec![0; header.payload_len()];
    recv.read_exact(&mut data).await.map_err(failed)?;
    let kind = header.kind().map_err(|_| failed("unknown frame"))?;
    Ok(Some((kind, data)))
}

/// A frame from the Node: binary frames carry Noise, a text frame is a refusal.
#[derive(uniffi::Record)]
pub struct LanFrame {
    pub text: bool,
    pub data: Vec<u8>,
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

    fn send(&self, kind: FrameKind, data: Vec<u8>) -> Result<()> {
        let header = encode_header(kind, data.len()).map_err(|_| failed("frame too large"))?;
        let bytes = self
            .bytes
            .clone()
            .try_acquire_many_owned((data.len() + HEADER_BYTES) as u32)
            .map_err(|_| failed("stream backpressure limit reached"))?;
        self.queue
            .try_send(QueuedFrame {
                header,
                data,
                _bytes: bytes,
            })
            .map_err(|error| match error {
                mpsc::error::TrySendError::Full(_) => failed("stream backpressure limit reached"),
                mpsc::error::TrySendError::Closed(_) => failed("stream closed"),
            })
    }
}

/// One client stream to a Node. Frames go out in call order; the Node's pings are answered here.
#[derive(uniffi::Object)]
pub struct LanStream {
    connection: Connection,
    outgoing: Outgoing,
    incoming: Mutex<RecvStream>,
}

impl LanStream {
    fn new(connection: Connection, mut send: SendStream, recv: RecvStream) -> Arc<Self> {
        let (outgoing, mut queue) = Outgoing::new();
        let writer = connection.clone();
        // Ends with the connection too: the app's handle, which holds the queue, lives until GC.
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
            send.finish().ok();
        });
        Arc::new(Self {
            connection,
            outgoing,
            incoming: Mutex::new(recv),
        })
    }
}

#[uniffi::export]
impl LanStream {
    /// Queues one binary frame. A failed write closes the stream, which `recv` then reports.
    pub fn send(&self, data: Vec<u8>) -> Result<()> {
        self.outgoing.send(FrameKind::Binary, data)
    }

    /// The next binary or text frame, `None` once the Node ended the stream.
    pub async fn recv(self: Arc<Self>) -> Result<Option<LanFrame>> {
        on_runtime(async move {
            let mut incoming = self.incoming.lock().await;
            loop {
                let Some((kind, data)) = read_frame(&mut incoming).await? else {
                    return Ok(None);
                };
                match kind {
                    FrameKind::Binary | FrameKind::Text => {
                        return Ok(Some(LanFrame {
                            text: kind == FrameKind::Text,
                            data,
                        }));
                    }
                    FrameKind::Ping => {
                        let _ = self.outgoing.send(FrameKind::Pong, data);
                    }
                    FrameKind::Pong => {}
                }
            }
        })
        .await
    }

    pub fn close(&self) {
        self.connection.close(0u32.into(), b"closed");
    }
}

/// Binds this device's endpoint ahead of the first dial, which then spends its time dialing.
#[uniffi::export]
pub async fn lan_prepare() -> Result<()> {
    on_runtime(async { endpoint().await.map(|_| ()) }).await
}

/// Opens one client stream to the Node `device_id` at `addrs` (`ip:port`); dialing it, not
/// binding this device's endpoint the first time, must finish within `timeout_ms`.
#[uniffi::export]
pub async fn lan_connect(
    device_id: String,
    addrs: Vec<String>,
    timeout_ms: u32,
) -> Result<Arc<LanStream>> {
    on_runtime(async move {
        secure::validate_device_id(&device_id)?;
        let id: EndpointId = device_id.parse().map_err(failed)?;
        let addrs: Vec<TransportAddr> = addrs
            .iter()
            .filter_map(|addr| addr.parse().ok())
            .map(TransportAddr::Ip)
            .collect();
        if addrs.is_empty() {
            return Err(failed("no LAN address"));
        }
        let endpoint = endpoint().await?;
        let dial = async {
            let connection = endpoint
                .connect(EndpointAddr::from_parts(id, addrs), ALPN)
                .await
                .map_err(failed)?;
            let (send, recv) = connection.open_bi().await.map_err(failed)?;
            Ok(LanStream::new(connection, send, recv))
        };
        tokio::time::timeout(Duration::from_millis(timeout_ms.into()), dial)
            .await
            .map_err(|_| failed("LAN connection timed out"))?
    })
    .await
}

/// Whether an advertised mDNS `tag` names `device_id` of `network_id`, by this device's clock.
#[uniffi::export]
pub fn lan_tag_matches(tag: String, network_id: String, device_id: String) -> bool {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.as_millis() as u64);
    secure::lan::lan_tag_matches(&tag, &network_id, &device_id, now)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Handshake, Identity};
    use iroh::SecretKey;
    use std::net::Ipv4Addr;

    const CONTEXT: &[u8] = b"vibeface:client:v1";

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

    #[test]
    fn outgoing_queue_holds_a_maximum_message_without_a_writer() {
        let sender = secure::Identity::generate().unwrap();
        let receiver = secure::Identity::generate().unwrap();
        let mut a = secure::Handshake::signed(true, &sender, None, CONTEXT).unwrap();
        let mut b = secure::Handshake::signed(false, &receiver, None, CONTEXT).unwrap();
        b.read(&a.write(&[]).unwrap()).unwrap();
        a.read(&b.write(&[]).unwrap()).unwrap();
        b.read(&a.write(&[]).unwrap()).unwrap();
        let mut a = a.finish().unwrap();
        let mut b = b.finish().unwrap();
        let payload = vec![0x3c; secure::MAX_APPLICATION_MESSAGE];
        let records = a.seal_message(&payload).unwrap();
        assert_eq!(records.len(), MAX_MESSAGE_RECORDS);
        assert_eq!(
            records
                .iter()
                .map(|record| record.len() + HEADER_BYTES)
                .sum::<usize>()
                + MAX_FRAME_BYTES
                + HEADER_BYTES,
            MAX_QUEUED_BYTES
        );
        let (outgoing, mut queued) = Outgoing::new();
        outgoing
            .send(FrameKind::Pong, vec![0; MAX_FRAME_BYTES])
            .unwrap();
        for record in records {
            outgoing.send(FrameKind::Binary, record).unwrap();
        }
        assert_eq!(outgoing.bytes.available_permits(), 0);
        assert!(outgoing.send(FrameKind::Binary, vec![]).is_err());
        assert_eq!(queued.try_recv().unwrap().header, [3, 0, 16, 0, 0]);
        let mut received = None;
        while let Ok(next) = queued.try_recv() {
            assert_eq!(
                decode_header(next.header).unwrap().kind(),
                Ok(FrameKind::Binary)
            );
            let message = b.open_record(&next.data).unwrap();
            if message.is_some() {
                assert!(received.is_none());
                received = message;
            }
        }
        assert_eq!(received.unwrap(), payload);
        assert_eq!(outgoing.bytes.available_permits(), MAX_QUEUED_BYTES);
    }

    #[test]
    fn outgoing_queue_bounds_bytes_and_releases_dropped_frames() {
        let (outgoing, mut queued) = Outgoing::new();
        assert!(
            outgoing
                .send(FrameKind::Binary, vec![0; MAX_FRAME_BYTES + 1])
                .is_err()
        );
        for _ in 0..MAX_QUEUED_BYTES / (MAX_FRAME_BYTES + HEADER_BYTES) {
            outgoing
                .send(FrameKind::Binary, vec![0; MAX_FRAME_BYTES])
                .unwrap();
        }
        assert!(
            outgoing
                .send(FrameKind::Binary, vec![0; MAX_FRAME_BYTES])
                .is_err()
        );
        drop(queued.try_recv().unwrap());
        outgoing
            .send(FrameKind::Binary, vec![0; MAX_FRAME_BYTES])
            .unwrap();
        drop(queued);
        assert_eq!(outgoing.bytes.available_permits(), MAX_QUEUED_BYTES);
        assert!(outgoing.send(FrameKind::Binary, vec![0]).is_err());
        assert_eq!(outgoing.bytes.available_permits(), MAX_QUEUED_BYTES);
    }

    /// A Node-like server on loopback: Noise as the responder, a ping the client must answer, then
    /// one sealed reply and the end of the stream.
    #[test]
    fn a_noise_session_runs_over_a_direct_stream() {
        RUNTIME.block_on(async {
            let node = secure::Identity::generate().unwrap();
            let seed: [u8; 32] = hex::decode(node.secret_hex()).unwrap().try_into().unwrap();
            let server = loopback_node(SecretKey::from_bytes(&seed)).await;
            let address = server.bound_sockets()[0].to_string();
            let serving = tokio::spawn(async move {
                // The first dial expects another key and gives up during the QUIC handshake.
                let connection = loop {
                    if let Ok(connection) = server.accept().await.unwrap().await {
                        break connection;
                    }
                };
                let (mut send, mut recv) = connection.accept_bi().await.unwrap();
                let mut handshake = secure::Handshake::signed(false, &node, None, CONTEXT).unwrap();
                let (kind, first) = read_frame(&mut recv).await.unwrap().unwrap();
                assert_eq!(kind, FrameKind::Binary);
                handshake.read(&first).unwrap();
                write_binary(&mut send, &handshake.write(&[]).unwrap()).await;
                let (kind, last) = read_frame(&mut recv).await.unwrap().unwrap();
                assert_eq!(kind, FrameKind::Binary);
                handshake.read(&last).unwrap();
                let mut channel = handshake.finish().unwrap();
                send.write_all(&[2, 0, 0, 0, 0]).await.unwrap(); // Empty Ping.
                let mut pong = [0; HEADER_BYTES];
                recv.read_exact(&mut pong).await.unwrap();
                assert_eq!(pong, [3, 0, 0, 0, 0]);
                for record in channel.seal_message(b"reply").unwrap() {
                    write_binary(&mut send, &record).await;
                }
                send.write_all(&[
                    3, 0, 0, 0, 1, 0xff, // Ignored Pong.
                    1, 0, 0, 0, 1, 0xff, // Text exposed as bytes.
                    4, 0, 0, 0, 1, 0xff, // Unknown frame with a complete payload.
                    0, 0, 0, 0, 0, // Empty Binary.
                ])
                .await
                .unwrap();
                send.finish().unwrap();
                connection.closed().await;
            });

            let phone = Identity::generate().unwrap();
            let unreachable = lan_connect(phone.device_id(), vec![address.clone()], 2000).await;
            assert!(unreachable.is_err(), "another device's key must not answer");
            let stream = lan_connect(node_id(&seed), vec![address], 5000)
                .await
                .unwrap();
            let client = Handshake::new(true, Some(node_id(&seed)), CONTEXT.to_vec()).unwrap();
            client
                .set_proof(
                    phone.device_id(),
                    phone.sign_message(client.signing_bytes()),
                )
                .unwrap();
            stream.send(client.write(Vec::new()).unwrap()).unwrap();
            let reply = stream.clone().recv().await.unwrap().unwrap();
            assert!(!reply.text);
            client.read(reply.data).unwrap();
            stream.send(client.write(Vec::new()).unwrap()).unwrap();
            let channel = client.finish().unwrap();
            let record = stream.clone().recv().await.unwrap().unwrap();
            assert_eq!(
                channel.open_record(record.data).unwrap(),
                Some(b"reply".to_vec())
            );
            let text = stream.clone().recv().await.unwrap().unwrap();
            assert!(text.text);
            assert_eq!(text.data, [0xff]);
            assert!(matches!(
                stream.clone().recv().await,
                Err(SecureError::Failed { reason }) if reason == "unknown frame"
            ));
            let empty = stream.clone().recv().await.unwrap().unwrap();
            assert!(!empty.text);
            assert!(empty.data.is_empty());
            assert!(stream.clone().recv().await.unwrap().is_none());
            stream.close();
            serving.await.unwrap();
        });
    }

    #[test]
    fn read_frame_rejects_partial_and_overlarge_frames() {
        RUNTIME.block_on(async {
            tokio::time::timeout(Duration::from_secs(5), async {
                let server = loopback_node(SecretKey::generate()).await;
                let address = EndpointAddr::from_parts(
                    server.id(),
                    [TransportAddr::Ip(server.bound_sockets()[0])],
                );
                let (dialed, accepted) =
                    tokio::join!(endpoint().await.unwrap().connect(address, ALPN), async {
                        server.accept().await.unwrap().await
                    });
                let sender = dialed.unwrap();
                let receiver = accepted.unwrap();
                for (wire, expected) in [
                    (&[0, 0][..], "stream finished early (2 bytes read)"),
                    (
                        &[4, 0, 0, 0, 2, 0xff],
                        "stream finished early (1 bytes read)",
                    ),
                    (&[4, 0, 16, 0, 1], "frame too large"),
                ] {
                    let (mut send, _) = sender.open_bi().await.unwrap();
                    send.write_all(wire).await.unwrap();
                    send.finish().unwrap();
                    let (_, mut recv) = receiver.accept_bi().await.unwrap();
                    assert!(matches!(
                        read_frame(&mut recv).await,
                        Err(SecureError::Failed { reason }) if reason == expected
                    ));
                }
            })
            .await
            .unwrap();
        });
    }

    fn node_id(seed: &[u8; 32]) -> String {
        hex::encode(SecretKey::from_bytes(seed).public().as_bytes())
    }

    #[test]
    fn tags_match_by_this_clock() {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;
        let tag = secure::lan::lan_tag("net", "dev", secure::lan::hour(now));
        assert!(lan_tag_matches(tag.clone(), "net".into(), "dev".into()));
        assert!(!lan_tag_matches(tag, "net".into(), "other".into()));
    }
}
