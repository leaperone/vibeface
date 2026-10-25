use anyhow::{Context, Result, ensure};
use futures_util::StreamExt;
use iroh::{
    Endpoint, EndpointAddr, EndpointId, RelayMap, RelayMode, SecretKey, TransportAddr,
    address_lookup::AddressLookup,
    endpoint::{PortmapperConfig, RecvStream, SendStream, presets},
};
use iroh_dns::endpoint_info::EndpointInfo;
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    net::{Ipv4Addr, SocketAddr},
    path::PathBuf,
    sync::{Arc, Mutex},
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use vibeface_secure::{Handshake, Identity};
use zeroize::Zeroizing;

mod discovery {
    include!(concat!(
        env!("VIBEFACE_PROBE_SOURCE_ROOT"),
        "/crates/node/src/net/discovery.rs"
    ));
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Input {
    actor_peer: String,
    identity_file: PathBuf,
    targets: Vec<Target>,
    order_id: String,
    #[serde(default)]
    action: Action,
}
#[derive(Default, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum Action {
    #[default]
    Security,
    Prefix {
        order: Value,
        prefix: String,
        expected_offset: u32,
    },
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Target {
    peer: String,
    address: SocketAddr,
    public_entry: bool,
}
impl Target {
    fn addr(&self) -> Result<EndpointAddr> {
        ensure!(
            self.address.ip().is_loopback() && self.address.port() != 0,
            "probe only dials explicit loopback targets"
        );
        let bytes: [u8; 32] = hex::decode(&self.peer)?
            .try_into()
            .map_err(|_| anyhow::anyhow!("expected peer length"))?;
        let id = EndpointId::from_bytes(&bytes)?;
        Ok(EndpointAddr::from_parts(
            id,
            [TransportAddr::Ip(self.address)],
        ))
    }
}
async fn packet(send: &mut SendStream, bytes: &[u8]) -> Result<()> {
    ensure!(
        bytes.len() <= 17408,
        "probe frame exceeds service control limit"
    );
    send.write_all(&(bytes.len() as u32).to_be_bytes()).await?;
    send.write_all(bytes).await?;
    Ok(())
}
async fn read_packet(recv: &mut RecvStream) -> Result<Vec<u8>> {
    let mut length = [0; 4];
    recv.read_exact(&mut length).await?;
    let length = u32::from_be_bytes(length) as usize;
    ensure!(
        (1..=17408).contains(&length),
        "unexpected probe response size"
    );
    let mut bytes = vec![0; length];
    recv.read_exact(&mut bytes).await?;
    Ok(bytes)
}
async fn service(
    endpoint: &Endpoint,
    identity: &Identity,
    target: &Target,
    request: Value,
) -> Result<Value> {
    let connection = endpoint
        .connect(target.addr()?, b"vibeface/service/1")
        .await?;
    let (mut send, mut recv) = connection.open_bi().await?;
    let mut handshake = Handshake::signed(
        true,
        identity,
        Some(target.peer.clone()),
        b"vibeface:service:v1",
    )?;
    packet(&mut send, &handshake.write(&[])?).await?;
    ensure!(
        handshake.read(&read_packet(&mut recv).await?)?.is_empty(),
        "unexpected handshake payload"
    );
    packet(&mut send, &handshake.write(&[])?).await?;
    ensure!(
        handshake.remote_id().as_deref() == Some(target.peer.as_str()),
        "service peer mismatch"
    );
    let mut channel = handshake.finish()?;
    for record in channel.seal_message(&serde_json::to_vec(&request)?)? {
        packet(&mut send, &record).await?;
    }
    send.finish()?;
    let value = channel
        .open_record(&read_packet(&mut recv).await?)?
        .context("response exceeds one record")?;
    let result = serde_json::from_slice(&value)?;
    connection.close(0u32.into(), b"probe complete");
    Ok(result)
}
async fn private_rejected(endpoint: &Endpoint, target: &Target, alpn: &[u8]) -> Result<()> {
    let closed = match endpoint.connect(target.addr()?, alpn).await {
        Ok(connection) => connection.closed().await.to_string(),
        Err(error) => error.to_string(),
    };
    let expected = if target.public_entry {
        "a public entry accepts no connections"
    } else {
        "not a member Node of this personal network"
    };
    ensure!(
        closed.contains(expected),
        "private ALPN did not return the membership boundary rejection"
    );
    Ok(())
}
async fn resolver_case(
    resolver: &discovery::FreshResolver,
    peer: EndpointId,
    accepted: bool,
) -> Result<()> {
    let mut stream = resolver.resolve(peer).context("resolver has no stream")?;
    let result = stream.next().await.context("resolver returned no result")?;
    if accepted {
        ensure!(
            result?.endpoint_id() == peer,
            "discovery resolved another peer"
        );
    } else {
        ensure!(result.is_err(), "invalid discovery packet was accepted");
    }
    Ok(())
}
async fn discovery_checks(key: &SecretKey) -> Result<Value> {
    let peer = key.public();
    let address = EndpointAddr::from_parts(
        peer,
        [TransportAddr::Ip(SocketAddr::from((
            Ipv4Addr::LOCALHOST,
            43001,
        )))],
    );
    let signed = EndpointInfo::from(address).to_pkarr_signed_packet(key, 30)?;
    let original = signed.to_relay_payload();
    let current = Arc::new(Mutex::new(original.clone()));
    let listener = tokio::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await?;
    let url = format!("http://{}/pkarr", listener.local_addr()?);
    let body = current.clone();
    let server = tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = listener.accept().await else {
                break;
            };
            let bytes = body.lock().expect("test packet").clone();
            tokio::spawn(async move {
                let mut request = [0; 4096];
                let Ok(length) = stream.read(&mut request).await else {
                    return;
                };
                if !request[..length].starts_with(b"GET /pkarr/") {
                    return;
                }
                let header = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/octet-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    bytes.len()
                );
                let _ = stream.write_all(header.as_bytes()).await;
                let _ = stream.write_all(&bytes).await;
            });
        }
    });
    let resolver = discovery::FreshResolver::new(url.parse()?)?;
    discovery::DiscoveryConfig {
        publish: false,
        resolve: true,
        urls: vec![url],
        publish_direct: false,
    }
    .validate()?;
    resolver_case(&resolver, peer, true).await?;
    let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_micros() as u64;
    for timestamp in [now - 1_800_000_000, now + 600_000_000] {
        let dns = &original[72..];
        let mut message = format!("3:seqi{timestamp}e1:v{}:", dns.len()).into_bytes();
        message.extend_from_slice(dns);
        let mut packet = key.sign(&message).to_bytes().to_vec();
        packet.extend_from_slice(&timestamp.to_be_bytes());
        packet.extend_from_slice(dns);
        *current.lock().expect("test packet") = packet;
        resolver_case(&resolver, peer, false).await?;
    }
    let mut bad = original;
    bad[0] ^= 1;
    *current.lock().expect("test packet") = bad;
    resolver_case(&resolver, peer, false).await?;
    *current.lock().expect("test packet") = vec![0; 1073];
    resolver_case(&resolver, peer, false).await?;
    server.abort();
    Ok(
        json!({"fresh":true,"expiredRejected":true,"futureRejected":true,"badSignatureRejected":true,"oversizeRejected":true,
        "productSourceSha256":hex::encode(Sha256::digest(include_bytes!(concat!(env!("VIBEFACE_PROBE_SOURCE_ROOT"), "/crates/node/src/net/discovery.rs"))))}),
    )
}
#[tokio::main]
async fn main() -> Result<()> {
    let args: Vec<_> = std::env::args().skip(1).collect();
    ensure!(args.len() == 1, "probe requires one private input file");
    let bytes = std::fs::read(&args[0])?;
    ensure!(bytes.len() <= 32768, "probe input exceeds limit");
    let input: Input = serde_json::from_slice(&bytes)?;
    ensure!(
        (1..=4).contains(&input.targets.len()),
        "probe requires bounded controlled targets"
    );
    ensure!(
        input.order_id.len() == 64 && input.order_id.bytes().all(|b| b.is_ascii_hexdigit()),
        "probe requires the controlled order id"
    );
    let metadata = std::fs::symlink_metadata(&input.identity_file)?;
    use std::os::unix::fs::PermissionsExt;
    ensure!(
        metadata.is_file()
            && !metadata.file_type().is_symlink()
            && metadata.permissions().mode() & 0o077 == 0,
        "probe identity must be a private regular file"
    );
    let identity_bytes = Zeroizing::new(std::fs::read(&input.identity_file)?);
    ensure!(identity_bytes.len() <= 1024, "identity file exceeds limit");
    #[derive(Deserialize)]
    struct StoredIdentity {
        secret_key_hex: String,
    }
    let stored: StoredIdentity = serde_json::from_slice(&identity_bytes)?;
    let secret = Zeroizing::new(stored.secret_key_hex);
    let identity = Identity::from_secret_hex(&secret)?;
    ensure!(
        identity.device_id() == input.actor_peer,
        "probe identity mismatches its expected actor peer"
    );
    let raw: [u8; 32] = hex::decode(secret.as_str())?
        .try_into()
        .map_err(|_| anyhow::anyhow!("identity length"))?;
    let key = SecretKey::from_bytes(&raw);
    let endpoint = Endpoint::builder(presets::Minimal)
        .secret_key(key.clone())
        .relay_mode(RelayMode::Custom(RelayMap::empty()))
        .portmapper_config(PortmapperConfig::Disabled)
        .clear_ip_transports()
        .bind_addr(SocketAddr::from((Ipv4Addr::LOCALHOST, 0)))?
        .bind()
        .await?;
    if let Action::Prefix {
        order,
        prefix,
        expected_offset,
    } = &input.action
    {
        ensure!(
            input.targets.len() == 1 && input.targets[0].public_entry,
            "prefix probe requires its controlled public entry"
        );
        let target = &input.targets[0];
        ensure!(
            order["terms"]["order_id"] == input.order_id
                && order["terms"]["source_peer"] == input.actor_peer
                && order["terms"]["provider_peer"] == target.peer,
            "prefix order does not bind the expected peers"
        );
        ensure!(
            prefix.len() == 10924 && [0, 8192].contains(expected_offset),
            "prefix probe requires one controlled 8192 byte chunk"
        );
        let call = async {
            let reserved = service(
                &endpoint,
                &identity,
                target,
                json!({"op":"charity","request":{"op":"reserve","order":order}}),
            )
            .await?;
            ensure!(
                reserved["error"].is_null() && reserved["result"]["order_id"] == input.order_id,
                "controlled signed reservation was rejected"
            );
            let offset_request = json!({"op":"charity","request":{"op":"offset","order_id":input.order_id,"index":0,"operation":"put_ciphertext"}});
            let before = service(&endpoint, &identity, target, offset_request.clone()).await?;
            ensure!(
                before["result"]["offset"] == *expected_offset,
                "durable prefix differs from its expected checkpoint"
            );
            let put = service(&endpoint, &identity, target, json!({"op":"charity","request":{"op":"put","order_id":input.order_id,"index":0,"operation":"put_ciphertext","offset":0,"bytes":prefix}})).await?;
            if *expected_offset == 0 {
                ensure!(
                    put["result"]["offset"] == 8192,
                    "prefix was not durably acknowledged"
                );
            } else {
                ensure!(
                    put["error"]
                        .as_str()
                        .is_some_and(|e| e.contains("durable prefix")),
                    "stale prefix upload was not rejected"
                );
            }
            let after = service(&endpoint, &identity, target, offset_request).await?;
            ensure!(
                after["result"]["offset"] == 8192,
                "prefix checkpoint changed after retry"
            );
            Ok::<_, anyhow::Error>(())
        };
        tokio::time::timeout(Duration::from_secs(45), call).await??;
        endpoint.close().await;
        println!(
            "{}",
            json!({"passed":true,"durablePrefix":8192,"afterRestart":*expected_offset == 8192,"staleOffsetRejected":*expected_offset == 8192})
        );
        return Ok(());
    }
    let mut cases = Vec::new();
    for target in &input.targets {
        let value = tokio::time::timeout(
            Duration::from_secs(15),
            service(
                &endpoint,
                &identity,
                target,
                json!({"op":"charity","request":{"op":"announce"}}),
            ),
        )
        .await??;
        ensure!(
            value.get("error").is_none() && value.get("result").is_some(),
            "positive service announcement failed"
        );
        for alpn in [
            b"vibeface/rpc/1".as_slice(),
            b"vibeface/sync/1",
            b"vibeface/blob/1",
        ] {
            tokio::time::timeout(
                Duration::from_secs(15),
                private_rejected(&endpoint, target, alpn),
            )
            .await??;
        }
        if target.public_entry {
            for (request, expected) in [
                (
                    json!({"op":"order","order_id":input.order_id}),
                    "charity order belongs to other Nodes",
                ),
                (
                    json!({"op":"get","order_id":input.order_id,"index":0,"operation":"get_ciphertext","offset":0}),
                    "charity grant client has the wrong role",
                ),
                (
                    json!({"op":"offset","order_id":input.order_id,"index":0,"operation":"put_ciphertext"}),
                    "charity grant client has the wrong role",
                ),
            ] {
                let value = tokio::time::timeout(
                    Duration::from_secs(15),
                    service(
                        &endpoint,
                        &identity,
                        target,
                        json!({"op":"charity","request":request}),
                    ),
                )
                .await??;
                ensure!(
                    value
                        .get("error")
                        .and_then(Value::as_str)
                        .is_some_and(|error| error.contains(expected)),
                    "foreign service operation did not reach its role boundary"
                );
            }
        }
        cases.push(json!({"peer":target.peer,"positiveService":true,"privateAlpnsRejected":3,"foreignObjectsRejected":target.public_entry}));
    }
    let discovery = discovery_checks(&key).await?;
    endpoint.close().await;
    println!(
        "{}",
        json!({"passed":true,"targets":cases,"discovery":discovery})
    );
    Ok(())
}
