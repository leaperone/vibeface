use anyhow::{Context, Result, ensure};
use futures_util::StreamExt;
use iroh::{
    Endpoint, EndpointAddr, EndpointId, RelayMap, RelayMode, SecretKey, TransportAddr,
    address_lookup::AddressLookup,
    endpoint::{PortmapperConfig, RecvStream, SendStream, presets},
};
use iroh_dns::endpoint_info::EndpointInfo;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    net::{Ipv4Addr, SocketAddr},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use vibeface_secure::membership::MembershipSigner;
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
    PaidUpload {
        offer: Value,
        ciphertext_grant: Value,
        capsule_grant: Value,
        prefix: String,
        tampered_capsule: String,
    },
    PaidResume {
        ciphertext_grant: Value,
        chunk: String,
    },
    PaidSecurity {
        scenario: PaidSecurity,
    },
    PaidOrder,
}
#[derive(Deserialize)]
#[serde(tag = "role", rename_all = "snake_case", deny_unknown_fields)]
enum PaidSecurity {
    Source,
    Receiver { provider_identity_file: PathBuf },
    Outsider { receiver_grant: Value },
}
#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct GrantBody {
    provider_peer: String,
    client_peer: String,
    order_id: String,
    window_index: u16,
    object_hash: String,
    operation: String,
    max_bytes: u32,
    expires_at: u64,
}
#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct SignedGrant {
    body: GrantBody,
    signature: String,
}
fn private_identity(path: &Path) -> Result<(Identity, Zeroizing<String>)> {
    use std::os::unix::fs::PermissionsExt;
    let metadata = std::fs::symlink_metadata(path)?;
    ensure!(
        metadata.is_file()
            && !metadata.file_type().is_symlink()
            && metadata.permissions().mode() & 0o077 == 0
            && metadata.len() <= 1024,
        "probe identity must be one private bounded regular file"
    );
    let bytes = Zeroizing::new(std::fs::read(path)?);
    #[derive(Deserialize)]
    struct StoredIdentity {
        secret_key_hex: String,
    }
    let value: StoredIdentity = serde_json::from_slice(&bytes)?;
    let secret = Zeroizing::new(value.secret_key_hex);
    Ok((Identity::from_secret_hex(&secret)?, secret))
}
fn rejection(value: &Value, expected: &str) -> Result<()> {
    ensure!(
        value["error"]
            .as_str()
            .is_some_and(|error| error.contains(expected)),
        "paid service did not reject specifically for its expected authority boundary"
    );
    Ok(())
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
    let (identity, secret) = private_identity(&input.identity_file)?;
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
    if let Action::PaidSecurity { scenario } = &input.action {
        let target = input.targets.last().context("paid provider missing")?;
        ensure!(
            target.public_entry,
            "paid security requires its actual public provider"
        );
        let call = async {
            let announced = service(&endpoint, &identity, target, json!({"op":"announce"})).await?;
            ensure!(
                announced["error"].is_null() && announced["result"]["paid_enabled"] == true,
                "paid service positive control cannot be replaced by a disabled charity request"
            );
            let mut checks = Vec::new();
            let mut extra = json!({});
            match scenario {
                PaidSecurity::Source => {
                    ensure!(
                        input.targets.len() == 1,
                        "source paid checks name only their fixed provider"
                    );
                    let order = service(
                        &endpoint,
                        &identity,
                        target,
                        json!({"op":"order","order_id":input.order_id}),
                    )
                    .await?;
                    ensure!(
                        order["error"].is_null() && order["result"]["order_id"] == input.order_id,
                        "the actual Source reads the existing completed provider order"
                    );
                    rejection(&service(&endpoint, &identity, target,
                        json!({"op":"grant","order_id":input.order_id,"index":0,"operation":"get_ciphertext"})).await?,
                        "service grant client has the wrong role")?;
                    checks.push("source-read-grant-wrong-role");
                }
                PaidSecurity::Receiver {
                    provider_identity_file,
                } => {
                    ensure!(
                        input.targets.len() == 1,
                        "receiver paid checks name only their fixed provider"
                    );
                    let order = service(
                        &endpoint,
                        &identity,
                        target,
                        json!({"op":"order","order_id":input.order_id}),
                    )
                    .await?;
                    ensure!(
                        order["error"].is_null() && order["result"]["order_id"] == input.order_id,
                        "the actual Receiver reads the same existing provider order"
                    );
                    let grant = service(&endpoint, &identity, target,
                        json!({"op":"grant","order_id":input.order_id,"index":0,"operation":"get_ciphertext"})).await?;
                    ensure!(
                        grant["error"].is_null(),
                        "the actual Receiver gets its live read grant"
                    );
                    let signed: SignedGrant = serde_json::from_value(grant["result"].clone())?;
                    ensure!(
                        signed.body.client_peer == input.actor_peer
                            && signed.body.provider_peer == target.peer
                            && signed.body.order_id == input.order_id
                            && signed.body.window_index == 0
                            && signed.body.operation == "get_ciphertext",
                        "the real read grant binds the designated receiver and exact original object"
                    );
                    vibeface_secure::verify(
                        &target.peer,
                        "delivery-grant",
                        &serde_json::to_vec(&signed.body)?,
                        &signed.signature,
                    )?;
                    let read = service(
                        &endpoint,
                        &identity,
                        target,
                        json!({"op":"get","grant":grant["result"],"offset":0}),
                    )
                    .await?;
                    let encoded = read["result"]["bytes"]
                        .as_str()
                        .context("real read returned no ciphertext bytes")?;
                    ensure!(
                        read["error"].is_null()
                            && read["result"]["next_offset"] == 8192
                            && encoded.len() == 10924,
                        "the valid receiver grant actually reads the existing complete ciphertext"
                    );
                    rejection(&service(&endpoint, &identity, target,
                        json!({"op":"grant","order_id":input.order_id,"index":0,"operation":"put_ciphertext"})).await?,
                        "service grant client has the wrong role")?;
                    let (provider, provider_secret) = private_identity(provider_identity_file)?;
                    ensure!(
                        provider.device_id() == target.peer,
                        "only this task's actual provider signs its finite expiry fixture"
                    );
                    let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
                    let mut expired: SignedGrant = serde_json::from_value(grant["result"].clone())?;
                    expired.body.expires_at = now
                        .checked_sub(1)
                        .context("host Unix clock is unavailable")?;
                    expired.signature = provider
                        .sign_payload("delivery-grant", &serde_json::to_vec(&expired.body)?)?;
                    vibeface_secure::verify(
                        &target.peer,
                        "delivery-grant",
                        &serde_json::to_vec(&expired.body)?,
                        &expired.signature,
                    )?;
                    drop(provider_secret);
                    rejection(
                        &service(
                            &endpoint,
                            &identity,
                            target,
                            json!({"op":"get","grant":expired,"offset":0}),
                        )
                        .await?,
                        "service grant expired",
                    )?;
                    use std::os::unix::fs::OpenOptionsExt;
                    let output = input
                        .identity_file
                        .with_file_name("paid-security-read-grant.json");
                    let bytes = serde_json::to_vec(&grant["result"])?;
                    ensure!(
                        bytes.len() <= 2048,
                        "the actual receiver grant fixture remains bounded"
                    );
                    let mut file = std::fs::OpenOptions::new()
                        .write(true)
                        .create_new(true)
                        .mode(0o600)
                        .open(output)?;
                    std::io::Write::write_all(&mut file, &bytes)?;
                    file.sync_all()?;
                    extra = json!({"receiver_grant_sha256":hex::encode(Sha256::digest(&bytes)),
                        "ciphertext_base64_sha256":hex::encode(Sha256::digest(encoded.as_bytes())),
                        "expired_signature_verified":true,"expiry_fixture":"resigned-provider-host-unix-seconds"});
                    checks.extend([
                        "receiver-valid-read",
                        "receiver-write-grant-wrong-role",
                        "signed-expired-grant",
                    ]);
                }
                PaidSecurity::Outsider { receiver_grant } => {
                    ensure!(
                        input.targets.len() == 3
                            && !input.targets[0].public_entry
                            && !input.targets[1].public_entry,
                        "the outsider checks exactly the real independent Source, Receiver and public Provider"
                    );
                    rejection(
                        &service(
                            &endpoint,
                            &identity,
                            target,
                            json!({"op":"order","order_id":input.order_id}),
                        )
                        .await?,
                        "delivery order belongs to another peer",
                    )?;
                    rejection(&service(&endpoint, &identity, target,
                        json!({"op":"grant","order_id":input.order_id,"index":0,"operation":"get_ciphertext"})).await?,
                        "service grant client has the wrong role")?;
                    rejection(
                        &service(
                            &endpoint,
                            &identity,
                            target,
                            json!({"op":"get","grant":receiver_grant,"offset":0}),
                        )
                        .await?,
                        "service grant belongs to another peer",
                    )?;
                    for node in &input.targets {
                        for alpn in [
                            b"vibeface/rpc/1".as_slice(),
                            b"vibeface/sync/1".as_slice(),
                            b"vibeface/blob/1".as_slice(),
                        ] {
                            private_rejected(&endpoint, node, alpn).await?;
                        }
                    }
                    checks.extend([
                        "outsider-order",
                        "outsider-grant-role",
                        "outsider-other-peer-grant",
                        "private-rpc-sync-blob-all-three-nodes",
                    ]);
                }
            }
            Ok::<_, anyhow::Error>(
                json!({"passed":true,"announcement":announced["result"],"checks":checks,"details":extra}),
            )
        };
        let observed = tokio::time::timeout(Duration::from_secs(90), call).await??;
        endpoint.close().await;
        println!("{observed}");
        return Ok(());
    }
    if matches!(&input.action, Action::PaidOrder) {
        ensure!(
            input.targets.len() == 1 && input.targets[0].public_entry,
            "paid order observation requires its controlled public provider"
        );
        let observed = tokio::time::timeout(
            Duration::from_secs(30),
            service(
                &endpoint,
                &identity,
                &input.targets[0],
                json!({"op":"order","order_id":input.order_id}),
            ),
        )
        .await??;
        ensure!(
            observed["error"].is_null() && observed["result"]["order_id"] == input.order_id,
            "authorized source cannot read its actual provider order view"
        );
        endpoint.close().await;
        println!("{}", json!({"passed":true,"order":observed["result"]}));
        return Ok(());
    }
    if let Action::PaidResume {
        ciphertext_grant,
        chunk,
    } = &input.action
    {
        ensure!(
            input.targets.len() == 1 && input.targets[0].public_entry,
            "paid resume uses its actual public provider"
        );
        let target = &input.targets[0];
        ensure!(
            chunk.len() == 10924
                && ciphertext_grant["body"]["order_id"] == input.order_id
                && ciphertext_grant["body"]["client_peer"] == input.actor_peer
                && ciphertext_grant["body"]["provider_peer"] == target.peer
                && ciphertext_grant["body"]["window_index"] == 0
                && ciphertext_grant["body"]["operation"] == "put_ciphertext",
            "paid resume binds the original signed grant, actor and next real ciphertext chunk"
        );
        let call = async {
            let offset = service(
                &endpoint,
                &identity,
                target,
                json!({"op":"offset","grant":ciphertext_grant}),
            )
            .await?;
            ensure!(
                offset["error"].is_null() && offset["result"]["offset"] == 8192,
                "provider restart changed the actual durable prefix"
            );
            let resumed = service(
                &endpoint,
                &identity,
                target,
                json!({"op":"put","grant":ciphertext_grant,"offset":8192,"bytes":chunk}),
            )
            .await?;
            ensure!(
                resumed["error"].is_null() && resumed["result"]["offset"] == 16384,
                "the first valid resumed append did not advance the original durable prefix"
            );
            Ok::<_, anyhow::Error>(())
        };
        tokio::time::timeout(Duration::from_secs(30), call).await??;
        endpoint.close().await;
        println!(
            "{}",
            json!({"passed":true,"resumed_offset":8192,"committed_offset":16384})
        );
        return Ok(());
    }
    if let Action::PaidUpload {
        offer,
        ciphertext_grant,
        capsule_grant,
        prefix,
        tampered_capsule,
    } = &input.action
    {
        ensure!(
            input.targets.len() == 1 && input.targets[0].public_entry,
            "paid upload probe requires its controlled public provider"
        );
        let target = &input.targets[0];
        ensure!(
            offer["op"] == "offer"
                && offer["mode"] == "paid"
                && prefix.len() == 10924
                && tampered_capsule.len() == 108,
            "paid probe uses its real offer, one bounded prefix and one capsule"
        );
        for (grant, operation) in [
            (ciphertext_grant, "put_ciphertext"),
            (capsule_grant, "put_capsule"),
        ] {
            ensure!(
                grant["body"]["order_id"] == input.order_id
                    && grant["body"]["client_peer"] == input.actor_peer
                    && grant["body"]["provider_peer"] == target.peer
                    && grant["body"]["window_index"] == 0
                    && grant["body"]["operation"] == operation,
                "paid upload grant binds its exact actor, provider, order, window and operation"
            );
        }
        let call = async {
            let offered = service(&endpoint, &identity, target, offer.clone()).await?;
            ensure!(
                offered["error"].is_null() && offered["result"]["order_id"] == input.order_id,
                "real funded paid offer was rejected"
            );
            let offset = |grant: &Value| json!({"op":"offset","grant":grant});
            let before = service(&endpoint, &identity, target, offset(ciphertext_grant)).await?;
            ensure!(
                before["result"]["offset"] == 0,
                "paid ciphertext must begin at the actual empty durable prefix"
            );
            for start in [1, 8192] {
                let value = service(
                    &endpoint,
                    &identity,
                    target,
                    json!({"op":"put","grant":ciphertext_grant,"offset":start,"bytes":prefix}),
                )
                .await?;
                ensure!(
                    value["error"]
                        .as_str()
                        .is_some_and(|error| error.contains("durable prefix")),
                    "paid wrong or reordered offset bypassed the actual durable prefix"
                );
            }
            let uploaded = service(
                &endpoint,
                &identity,
                target,
                json!({"op":"put","grant":ciphertext_grant,"offset":0,"bytes":prefix}),
            )
            .await?;
            ensure!(
                uploaded["result"]["offset"] == 8192,
                "real paid ciphertext prefix was not durably acknowledged"
            );
            for start in [0, 16384] {
                let value = service(
                    &endpoint,
                    &identity,
                    target,
                    json!({"op":"put","grant":ciphertext_grant,"offset":start,"bytes":prefix}),
                )
                .await?;
                ensure!(
                    value["error"]
                        .as_str()
                        .is_some_and(|error| error.contains("durable prefix")),
                    "paid duplicate or reordered chunk changed its durable prefix"
                );
            }
            let after = service(&endpoint, &identity, target, offset(ciphertext_grant)).await?;
            ensure!(
                after["result"]["offset"] == 8192,
                "rejected paid chunks changed the durable ciphertext prefix"
            );
            let before_capsule =
                service(&endpoint, &identity, target, offset(capsule_grant)).await?;
            ensure!(
                before_capsule["result"]["offset"] == 0,
                "paid capsule must begin empty before its hash check"
            );
            let tampered = service(
                &endpoint,
                &identity,
                target,
                json!({"op":"put","grant":capsule_grant,"offset":0,"bytes":tampered_capsule}),
            )
            .await?;
            ensure!(
                tampered["error"]
                    .as_str()
                    .is_some_and(|error| error.contains("complete delivery object hash mismatch")),
                "tampered paid capsule did not reach the actual complete-object hash check"
            );
            let after_capsule =
                service(&endpoint, &identity, target, offset(capsule_grant)).await?;
            ensure!(
                after_capsule["result"]["offset"] == 0,
                "a rejected paid capsule retained an invalid durable completion"
            );
            let observed = service(
                &endpoint,
                &identity,
                target,
                json!({"op":"order","order_id":input.order_id}),
            )
            .await?;
            let order = &observed["result"];
            ensure!(
                observed["error"].is_null()
                    && order["payment"]["earned"] == "0"
                    && order["payment"]["withdrawn"] == "0"
                    && order["evidence"] == "none"
                    && order["windows"].as_array().is_some_and(|windows| windows
                        .iter()
                        .all(|window| window["decryption_proven"] == false)),
                "rejected paid content acquired early evidence or payment"
            );
            Ok::<_, anyhow::Error>(())
        };
        tokio::time::timeout(Duration::from_secs(90), call).await??;
        endpoint.close().await;
        println!(
            "{}",
            json!({"passed":true,"paid":true,"durablePrefix":8192,
            "wrongOffsetRejected":true,"reorderedRejected":true,"duplicateRejected":true,
            "capsuleHashMismatchRejected":true,"capsuleOffsetAfterRejection":0,"earlyEarned":"0","earlyWithdrawn":"0"})
        );
        return Ok(());
    }
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
