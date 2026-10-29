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
    sync::{Mutex, OnceCell, mpsc},
};
use vibeface_secure as secure;

use crate::{Result, SecureError};

const ALPN: &[u8] = b"vibeface/client/1";
/// What a Node accepts in one frame.
const MAX_FRAME: usize = 1024 * 1024;
const BINARY: u8 = 0;
const TEXT: u8 = 1;
const PING: u8 = 2;
const PONG: u8 = 3;

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

fn frame(tag: u8, data: &[u8]) -> Vec<u8> {
    let mut frame = Vec::with_capacity(5 + data.len());
    frame.push(tag);
    frame.extend_from_slice(&(data.len() as u32).to_be_bytes());
    frame.extend_from_slice(data);
    frame
}

/// `None` once the Node finished the stream between frames.
async fn read_frame(recv: &mut RecvStream) -> Result<Option<(u8, Vec<u8>)>> {
    let mut header = [0u8; 5];
    match recv.read_exact(&mut header).await {
        Ok(()) => {}
        Err(ReadExactError::FinishedEarly(0)) => return Ok(None),
        Err(error) => return Err(failed(error)),
    }
    let length = u32::from_be_bytes([header[1], header[2], header[3], header[4]]) as usize;
    if length > MAX_FRAME {
        return Err(failed("frame too large"));
    }
    let mut data = vec![0; length];
    recv.read_exact(&mut data).await.map_err(failed)?;
    Ok(Some((header[0], data)))
}

/// A frame from the Node: binary frames carry Noise, a text frame is a refusal.
#[derive(uniffi::Record)]
pub struct LanFrame {
    pub text: bool,
    pub data: Vec<u8>,
}

/// One client stream to a Node. Frames go out in call order; the Node's pings are answered here.
#[derive(uniffi::Object)]
pub struct LanStream {
    connection: Connection,
    outgoing: mpsc::UnboundedSender<(u8, Vec<u8>)>,
    incoming: Mutex<RecvStream>,
}

impl LanStream {
    fn new(connection: Connection, mut send: SendStream, recv: RecvStream) -> Arc<Self> {
        let (outgoing, mut queue) = mpsc::unbounded_channel::<(u8, Vec<u8>)>();
        let writer = connection.clone();
        // Ends with the connection too: the app's handle, which holds the queue, lives until GC.
        tokio::spawn(async move {
            loop {
                let next = tokio::select! {
                    next = queue.recv() => next,
                    _ = writer.closed() => return,
                };
                let Some((tag, data)) = next else { break };
                if send.write_all(&frame(tag, &data)).await.is_err() {
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
        if data.len() > MAX_FRAME {
            return Err(failed("frame too large"));
        }
        self.outgoing
            .send((BINARY, data))
            .map_err(|_| failed("stream closed"))
    }

    /// The next binary or text frame, `None` once the Node ended the stream.
    pub async fn recv(self: Arc<Self>) -> Result<Option<LanFrame>> {
        on_runtime(async move {
            let mut incoming = self.incoming.lock().await;
            loop {
                let Some((tag, data)) = read_frame(&mut incoming).await? else {
                    return Ok(None);
                };
                match tag {
                    BINARY | TEXT => {
                        return Ok(Some(LanFrame {
                            text: tag == TEXT,
                            data,
                        }));
                    }
                    PING => {
                        let _ = self.outgoing.send((PONG, data));
                    }
                    PONG => {}
                    _ => return Err(failed("unknown frame")),
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

    const CONTEXT: &[u8] = b"vibeface:client:v1";

    #[test]
    fn frames_carry_tag_and_big_endian_length() {
        assert_eq!(frame(TEXT, b"no"), [1, 0, 0, 0, 2, b'n', b'o']);
        assert_eq!(frame(BINARY, &[]), [0, 0, 0, 0, 0]);
    }

    /// A Node-like server on loopback: Noise as the responder, a ping the client must answer, then
    /// one sealed reply and the end of the stream.
    #[test]
    fn a_noise_session_runs_over_a_direct_stream() {
        RUNTIME.block_on(async {
            let node = secure::Identity::generate().unwrap();
            let seed: [u8; 32] = hex::decode(node.secret_hex()).unwrap().try_into().unwrap();
            let server = Endpoint::builder(presets::Minimal)
                .secret_key(SecretKey::from_bytes(&seed))
                .alpns(vec![ALPN.to_vec()])
                .relay_mode(RelayMode::Disabled)
                .clear_ip_transports()
                .bind_addr((std::net::Ipv4Addr::LOCALHOST, 0))
                .unwrap()
                .bind()
                .await
                .unwrap();
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
                let (_, first) = read_frame(&mut recv).await.unwrap().unwrap();
                handshake.read(&first).unwrap();
                send.write_all(&frame(BINARY, &handshake.write(&[]).unwrap()))
                    .await
                    .unwrap();
                let (_, last) = read_frame(&mut recv).await.unwrap().unwrap();
                handshake.read(&last).unwrap();
                let mut channel = handshake.finish().unwrap();
                send.write_all(&frame(PING, &[])).await.unwrap();
                assert_eq!(read_frame(&mut recv).await.unwrap(), Some((PONG, vec![])));
                for record in channel.seal_message(b"reply").unwrap() {
                    send.write_all(&frame(BINARY, &record)).await.unwrap();
                }
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
            assert!(stream.clone().recv().await.unwrap().is_none());
            stream.close();
            serving.await.unwrap();
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
