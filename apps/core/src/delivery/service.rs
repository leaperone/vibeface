use super::{
    Delivery,
    types::{MAX_CONTROL_BYTES, ServiceRequest},
};
use crate::{net, runtime::Runtime};
use anyhow::{Context, Result, ensure};
use iroh::endpoint::{Connection, RecvStream, SendStream};
use serde_json::{Value, json};
use std::{sync::Arc, time::Duration};
use vibeface_secure::Channel;

const CONTEXT: &[u8] = b"vibeface:service:v1";

async fn write_packet(send: &mut SendStream, bytes: &[u8], limit: usize) -> Result<()> {
    ensure!(bytes.len() <= limit, "service frame exceeds limit");
    send.write_all(&(bytes.len() as u32).to_be_bytes()).await?;
    send.write_all(bytes).await?;
    Ok(())
}
async fn read_packet(recv: &mut RecvStream, limit: usize) -> Result<Vec<u8>> {
    let mut header = [0u8; 4];
    recv.read_exact(&mut header).await?;
    let length = u32::from_be_bytes(header) as usize;
    ensure!(length > 0 && length <= limit, "service frame exceeds limit");
    let mut body = vec![0; length];
    recv.read_exact(&mut body).await?;
    Ok(body)
}
async fn write_value(send: &mut SendStream, channel: &mut Channel, value: &Value) -> Result<()> {
    let bytes = serde_json::to_vec(value)?;
    ensure!(
        bytes.len() <= MAX_CONTROL_BYTES,
        "service control message exceeds limit"
    );
    for record in channel.seal_message(&bytes)? {
        write_packet(send, &record, MAX_CONTROL_BYTES + 1024).await?;
    }
    Ok(())
}
async fn read_value(recv: &mut RecvStream, channel: &mut Channel) -> Result<Value> {
    let record = read_packet(recv, MAX_CONTROL_BYTES + 1024).await?;
    let value = channel
        .open_record(&record)?
        .context("service message must fit one record")?;
    ensure!(
        value.len() <= MAX_CONTROL_BYTES,
        "service control message exceeds limit"
    );
    Ok(serde_json::from_slice(&value)?)
}
pub async fn request(node: &Runtime, peer: &str, request: &ServiceRequest) -> Result<Value> {
    let connection = node.net()?.connect(peer, net::SERVICE).await?;
    tokio::time::timeout(Duration::from_secs(120), async {
        let (mut send, mut recv) = connection.open_bi().await?;
        let mut handshake = node
            .identity
            .handshake(true, Some(peer.to_owned()), CONTEXT)?;
        write_packet(&mut send, &handshake.write(&[])?, 4096).await?;
        ensure!(
            handshake
                .read(&read_packet(&mut recv, 4096).await?)?
                .is_empty(),
            "unexpected service handshake payload"
        );
        write_packet(&mut send, &handshake.write(&[])?, 4096).await?;
        let mut channel = handshake.finish()?;
        write_value(&mut send, &mut channel, &serde_json::to_value(request)?).await?;
        send.finish()?;
        let response = read_value(&mut recv, &mut channel).await?;
        if let Some(error) = response.get("error").and_then(Value::as_str) {
            anyhow::bail!("{error}");
        }
        response
            .get("result")
            .cloned()
            .context("invalid service response")
    })
    .await
    .context("service outcome is unknown; reconcile before retrying")?
}

impl Delivery {
    pub(crate) async fn serve(&self, node: Arc<Runtime>, connection: Connection) -> Result<()> {
        ensure!(self.config().enabled, "object delivery service is disabled");
        let _global = self
            .service_slots
            .try_acquire()
            .context("service connection budget exhausted")?;
        let remote = net::device_id(&connection.remote_id());
        let peer_slots = {
            let mut slots = self.peer_slots.lock().expect("service peer slots");
            slots.retain(|_, slot| slot.strong_count() > 0);
            match slots.get(&remote).and_then(std::sync::Weak::upgrade) {
                Some(slot) => slot,
                None => {
                    let slot = Arc::new(tokio::sync::Semaphore::new(2));
                    slots.insert(remote.clone(), Arc::downgrade(&slot));
                    slot
                }
            }
        };
        let _peer = peer_slots
            .try_acquire()
            .context("peer service budget exhausted")?;
        for _ in 0..64 {
            let (mut send, mut recv) =
                match tokio::time::timeout(Duration::from_secs(30), connection.accept_bi()).await {
                    Ok(Ok(streams)) => streams,
                    _ => break,
                };
            let result = tokio::time::timeout(Duration::from_secs(120), async {
                ensure!(
                    self.config().enabled,
                    "object delivery service was disabled"
                );
                let mut handshake =
                    node.identity
                        .handshake(false, Some(remote.clone()), CONTEXT)?;
                ensure!(
                    handshake
                        .read(&read_packet(&mut recv, 4096).await?)?
                        .is_empty(),
                    "unexpected service handshake payload"
                );
                write_packet(&mut send, &handshake.write(&[])?, 4096).await?;
                ensure!(
                    handshake
                        .read(&read_packet(&mut recv, 4096).await?)?
                        .is_empty(),
                    "unexpected service handshake payload"
                );
                let mut channel = handshake.finish()?;
                let value = read_value(&mut recv, &mut channel).await?;
                let request: ServiceRequest = serde_json::from_value(value)?;
                let response = match self.handle_service(&node, &remote, request).await {
                    Ok(result) => json!({"result":result}),
                    Err(error) => json!({"error":error.to_string()}),
                };
                write_value(&mut send, &mut channel, &response).await?;
                send.finish()?;
                Ok::<_, anyhow::Error>(())
            })
            .await;
            if !matches!(result, Ok(Ok(()))) {
                connection.close(1u32.into(), b"service request rejected");
                break;
            }
        }
        connection.close(0u32.into(), b"service session finished");
        Ok(())
    }
}
