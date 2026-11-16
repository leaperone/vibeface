//! Noise/WebSocket transport for controlled native-client test responders.
use crate::identity::PROLOGUE;
use anyhow::{Context, Result, bail, ensure};
use futures_util::{SinkExt, StreamExt};
use serde_json::Value;
use vibeface_secure::{Channel, Handshake, Identity};

type ServerSocket = tokio_tungstenite::WebSocketStream<tokio::net::TcpStream>;
pub(crate) async fn server(
    listener: &tokio::net::TcpListener,
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
