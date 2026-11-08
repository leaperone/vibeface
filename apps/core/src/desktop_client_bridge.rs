//! A client-only stdio adapter for Desktop's existing iroh/Noise transport.
//! This runs before config, identity, storage, logging, or Node startup.

use anyhow::{Context, Result, bail, ensure};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde::{Deserialize, Serialize};
use std::{net::SocketAddr, sync::Arc};
use tokio::{
    io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader},
    sync::mpsc,
};
use vibeface_secure_ffi::lan::{LanFrame, LanStream, MAX_FRAME_BYTES, lan_connect};

const MAX_ADDRESSES: usize = 16;
const MAX_TIMEOUT_MS: u32 = 30_000;
const MAX_LINE_BYTES: usize = MAX_FRAME_BYTES * 4 / 3 + 4096;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct OpenRequest {
    #[serde(rename = "type")]
    kind: String,
    device_id: String,
    addrs: Vec<String>,
    timeout_ms: u32,
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
enum Command {
    Send { data: String },
    Close {},
}

#[derive(Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum Output<'a> {
    Opened,
    Sent,
    Frame { text: bool, data: String },
    Closed,
    Error { message: &'a str },
}

enum ReadEvent {
    Frame(LanFrame),
    Closed,
    Error,
}

async fn read_bounded_line<R: AsyncRead + Unpin>(
    reader: &mut BufReader<R>,
) -> Result<Option<Vec<u8>>> {
    let mut line = Vec::new();
    loop {
        let available = reader.fill_buf().await?;
        if available.is_empty() {
            return if line.is_empty() {
                Ok(None)
            } else {
                Ok(Some(line))
            };
        }
        let newline = available.iter().position(|byte| *byte == b'\n');
        let count = newline.map_or(available.len(), |index| index + 1);
        ensure!(
            line.len().saturating_add(count) <= MAX_LINE_BYTES,
            "bridge message exceeds its size limit"
        );
        line.extend_from_slice(&available[..count]);
        reader.consume(count);
        if newline.is_some() {
            line.pop();
            return Ok(Some(line));
        }
    }
}

async fn write_output<W: AsyncWrite + Unpin>(writer: &mut W, value: Output<'_>) -> Result<()> {
    let mut bytes = serde_json::to_vec(&value)?;
    ensure!(
        bytes.len() <= MAX_LINE_BYTES,
        "bridge response exceeds its size limit"
    );
    bytes.push(b'\n');
    writer.write_all(&bytes).await?;
    writer.flush().await?;
    Ok(())
}

fn numeric_addresses(addresses: Vec<String>) -> Result<Vec<String>> {
    ensure!(
        !addresses.is_empty() && addresses.len() <= MAX_ADDRESSES,
        "invalid LAN address count"
    );
    let mut unique = Vec::with_capacity(addresses.len());
    for address in addresses {
        let socket: SocketAddr = address
            .parse()
            .context("LAN address must be numeric IP:port")?;
        ensure!(socket.port() != 0, "LAN port is invalid");
        let canonical = socket.to_string();
        if !unique.contains(&canonical) {
            unique.push(canonical);
        }
    }
    ensure!(!unique.is_empty(), "no valid LAN address");
    Ok(unique)
}

async fn receive(stream: Arc<LanStream>, tx: mpsc::Sender<ReadEvent>) {
    loop {
        match stream.clone().recv().await {
            Ok(Some(frame)) => {
                if tx.send(ReadEvent::Frame(frame)).await.is_err() {
                    return;
                }
            }
            Ok(None) => {
                let _ = tx.send(ReadEvent::Closed).await;
                return;
            }
            Err(_) => {
                let _ = tx.send(ReadEvent::Error).await;
                return;
            }
        }
    }
}

pub async fn run() -> Result<()> {
    let stdin = tokio::io::stdin();
    let stdout = tokio::io::stdout();
    run_io(stdin, stdout).await
}

async fn run_io<R: AsyncRead + Unpin, W: AsyncWrite + Unpin>(
    input: R,
    mut output: W,
) -> Result<()> {
    let mut input = BufReader::new(input);
    let Some(line) = read_bounded_line(&mut input).await? else {
        bail!("missing LAN open request")
    };
    let open: OpenRequest = serde_json::from_slice(&line).context("invalid LAN open request")?;
    ensure!(
        open.kind == "open",
        "first bridge request must open a stream"
    );
    ensure!(
        open.timeout_ms > 0 && open.timeout_ms <= MAX_TIMEOUT_MS,
        "invalid LAN timeout"
    );
    ensure!(
        open.device_id.len() == 64
            && open
                .device_id
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase()),
        "invalid Node identity"
    );
    let addrs = numeric_addresses(open.addrs)?;
    let stream = lan_connect(open.device_id, addrs, open.timeout_ms)
        .await
        .map_err(|_| anyhow::anyhow!("LAN connection failed"))?;
    write_output(&mut output, Output::Opened).await?;

    let (tx, mut events) = mpsc::channel(16);
    tokio::spawn(receive(stream.clone(), tx));
    loop {
        tokio::select! {
            line = read_bounded_line(&mut input) => {
                let Some(line) = line? else { stream.close(); return Ok(()) };
                let command: Command = serde_json::from_slice(&line).context("invalid LAN bridge command")?;
                match command {
                    Command::Send { data } => {
                        ensure!(data.len() <= MAX_FRAME_BYTES * 4 / 3 + 4, "LAN frame exceeds its size limit");
                        let frame = STANDARD.decode(data).context("invalid LAN frame encoding")?;
                        ensure!(frame.len() <= MAX_FRAME_BYTES, "LAN frame exceeds its size limit");
                        stream.send(frame).map_err(|_| anyhow::anyhow!("LAN stream is closed or full"))?;
                        write_output(&mut output, Output::Sent).await?;
                    }
                    Command::Close {} => {
                        stream.close();
                        write_output(&mut output, Output::Closed).await?;
                        return Ok(());
                    }
                }
            }
            event = events.recv() => {
                match event {
                    Some(ReadEvent::Frame(frame)) => write_output(&mut output, Output::Frame { text: frame.text, data: STANDARD.encode(frame.data) }).await?,
                    Some(ReadEvent::Closed) => { write_output(&mut output, Output::Closed).await?; stream.close(); return Ok(()) }
                    Some(ReadEvent::Error) | None => { write_output(&mut output, Output::Error { message: "LAN stream failed" }).await?; stream.close(); return Ok(()) }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use iroh::{Endpoint, RelayMode, SecretKey, endpoint::presets};
    use tokio::io::duplex;
    use vibeface_secure::Identity;

    const ALPN: &[u8] = b"vibeface/client/1";

    async fn read_wire_frame(recv: &mut iroh::endpoint::RecvStream) -> Result<Vec<u8>> {
        let mut header = [0; 5];
        recv.read_exact(&mut header).await?;
        let length = u32::from_be_bytes(header[1..5].try_into()?) as usize;
        ensure!(length <= MAX_FRAME_BYTES, "fixture frame exceeds limit");
        let mut bytes = vec![0; length];
        recv.read_exact(&mut bytes).await?;
        ensure!(header[0] == 0, "fixture expected a binary frame");
        Ok(bytes)
    }

    fn wire_frame(bytes: &[u8]) -> Vec<u8> {
        let mut framed = Vec::with_capacity(5 + bytes.len());
        framed.push(0);
        framed.extend_from_slice(&(bytes.len() as u32).to_be_bytes());
        framed.extend_from_slice(bytes);
        framed
    }

    async fn response_line<R: AsyncRead + Unpin>(
        reader: &mut BufReader<R>,
    ) -> Result<serde_json::Value> {
        let line = read_bounded_line(reader)
            .await?
            .context("bridge response missing")?;
        Ok(serde_json::from_slice(&line)?)
    }

    #[test]
    fn numeric_addresses_are_bounded_and_deduplicated() {
        assert_eq!(
            numeric_addresses(vec!["127.0.0.1:1".into(), "127.0.0.1:1".into()]).unwrap(),
            ["127.0.0.1:1"]
        );
        assert!(numeric_addresses(vec!["node.local:1".into()]).is_err());
        assert!(
            numeric_addresses(
                (0..MAX_ADDRESSES + 1)
                    .map(|port| format!("127.0.0.1:{}", port + 1))
                    .collect()
            )
            .is_err()
        );
    }

    #[tokio::test]
    async fn bridge_refuses_non_open_input_before_network_setup() {
        let (input, mut writer) = duplex(256);
        writer.write_all(b"{\"type\":\"close\"}\n").await.unwrap();
        drop(writer);
        assert!(run_io(input, tokio::io::sink()).await.is_err());
    }

    #[tokio::test]
    async fn stdio_bridge_preserves_lan_frame_bytes_and_closes_on_close_command() -> Result<()> {
        let node = Identity::generate()?;
        let seed: [u8; 32] = hex::decode(node.secret_hex())?
            .try_into()
            .map_err(|_| anyhow::anyhow!("invalid identity seed"))?;
        let endpoint = Endpoint::builder(presets::Minimal)
            .secret_key(SecretKey::from_bytes(&seed))
            .alpns(vec![ALPN.to_vec()])
            .relay_mode(RelayMode::Disabled)
            .clear_ip_transports()
            .bind_addr((std::net::Ipv4Addr::LOCALHOST, 0))?
            .bind()
            .await?;
        let address = endpoint.bound_sockets()[0].to_string();
        let node_id = node.device_id();
        let expected = vec![0, 1, 2, 0x7f, 0x80, 0xfe, 0xff];
        let serving = tokio::spawn({
            let endpoint = endpoint.clone();
            let expected = expected.clone();
            async move {
                let connection = endpoint
                    .accept()
                    .await
                    .context("fixture connection missing")?
                    .await?;
                let (mut send, mut recv) = connection.accept_bi().await?;
                let input = read_wire_frame(&mut recv).await?;
                anyhow::ensure!(input == expected, "bridge changed outbound LAN frame");
                send.write_all(&wire_frame(&expected)).await?;
                connection.closed().await;
                anyhow::Ok(())
            }
        });

        let (bridge_input, mut input_writer) = duplex(MAX_LINE_BYTES * 2);
        let (mut output_reader, bridge_output) = duplex(MAX_LINE_BYTES * 2);
        let bridge = tokio::spawn(run_io(bridge_input, bridge_output));
        let mut output = BufReader::new(&mut output_reader);
        input_writer
            .write_all(
                serde_json::to_string(&serde_json::json!({
                    "type": "open", "device_id": node_id, "addrs": [address], "timeout_ms": 5000
                }))?
                .as_bytes(),
            )
            .await?;
        input_writer.write_all(b"\n").await?;
        anyhow::ensure!(response_line(&mut output).await?["type"] == "opened");
        input_writer
            .write_all(
                serde_json::to_string(&serde_json::json!({
                    "type": "send", "data": STANDARD.encode(&expected)
                }))?
                .as_bytes(),
            )
            .await?;
        input_writer.write_all(b"\n").await?;
        let mut acknowledged = false;
        let mut echoed = false;
        for _ in 0..2 {
            let message = response_line(&mut output).await?;
            match message["type"].as_str() {
                Some("sent") => acknowledged = true,
                Some("frame") => {
                    let data =
                        STANDARD.decode(message["data"].as_str().context("frame data missing")?)?;
                    anyhow::ensure!(data == expected, "bridge changed inbound LAN frame");
                    echoed = true;
                }
                other => bail!("unexpected bridge reply: {other:?}"),
            }
        }
        anyhow::ensure!(
            acknowledged && echoed,
            "bridge did not send and return the LAN frame"
        );
        input_writer.write_all(b"{\"type\":\"close\"}\n").await?;
        anyhow::ensure!(response_line(&mut output).await?["type"] == "closed");
        drop(input_writer);
        bridge.await??;
        serving.await??;
        endpoint.close().await;
        Ok(())
    }
}
