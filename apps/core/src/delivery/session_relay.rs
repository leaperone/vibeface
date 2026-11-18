//! Store-and-forward of opaque objects over the authenticated delivery service.

use super::{
    Delivery, service,
    session::{SIGNING_DOMAIN, SessionStoredReceipt, verify_object},
    session_relay_store::{RELAY_CHUNK_BYTES, RelayManifest, RelayProgress, SessionRelayStore},
    types::{Mode, ServiceRequest, parse_word},
};
use crate::runtime::Runtime;
use anyhow::{Context, Result, ensure};
use base64::{Engine, engine::general_purpose::STANDARD};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::{Value, json};
use vibeface_delivery::{
    CAPSULE_BYTES, EncryptedSessionObject, SESSION_AEAD_TAG_BYTES, Word, hash, verify_peer,
};

const MAX_RETAIN_MS: u64 = 24 * 60 * 60 * 1000;

#[derive(Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
pub(super) enum SessionRelayRequest {
    Begin {
        manifest: Box<RelayManifest>,
    },
    Put {
        object_id: Word,
        offset: u64,
        bytes: String,
    },
    Manifest {
        object_id: Word,
    },
    Get {
        object_id: Word,
        offset: u64,
    },
    Acknowledge {
        object_id: Word,
        receipt: SessionStoredReceipt,
    },
    Receipt {
        object_id: Word,
    },
}

fn now_ms() -> Result<u64> {
    Ok(crate::clock::now_ms().try_into()?)
}

fn capsule_from(value: &str) -> Result<[u8; CAPSULE_BYTES]> {
    ensure!(
        value.len() == 4 * CAPSULE_BYTES.div_ceil(3),
        "invalid session capsule length"
    );
    let bytes = STANDARD.decode(value)?;
    ensure!(
        STANDARD.encode(&bytes) == value,
        "noncanonical session capsule"
    );
    bytes
        .try_into()
        .ok()
        .context("invalid session capsule length")
}

fn verify_manifest(
    manifest: &RelayManifest,
    provider: &Word,
    at: u64,
) -> Result<[u8; CAPSULE_BYTES]> {
    let header = &manifest.header;
    ensure!(&header.provider_peer == provider, "wrong session relay");
    ensure!(
        header.created_at_ms <= at
            && at < header.expires_at_ms
            && header.expires_at_ms - header.created_at_ms <= MAX_RETAIN_MS,
        "session relay manifest is outside its retention interval"
    );
    ensure!(
        header.payload_bytes > (64 + 12 + SESSION_AEAD_TAG_BYTES) as u64
            && header.chunk_count as u64 == header.payload_bytes.div_ceil(RELAY_CHUNK_BYTES as u64),
        "invalid session relay chunk layout"
    );
    verify_peer(
        &header.sender_peer,
        SIGNING_DOMAIN,
        &header.signing_bytes()?,
        &manifest.signature,
    )?;
    capsule_from(&manifest.capsule)
}

async fn request<T: DeserializeOwned>(
    node: &Runtime,
    relay: &Word,
    request: SessionRelayRequest,
) -> Result<T> {
    let value = service::request(
        node,
        &hex::encode(relay),
        &ServiceRequest::SessionRelay { request },
    )
    .await?;
    Ok(serde_json::from_value(value)?)
}

impl Delivery {
    pub(super) fn handle_session_relay(
        &self,
        remote: &str,
        request: SessionRelayRequest,
    ) -> Result<Value> {
        self.ensure_mode(Mode::Charity)?;
        let config = self.config();
        ensure!(config.session_relay_enabled, "session relay is disabled");
        let remote = parse_word(remote)?;
        let provider = parse_word(&self.identity.device_id()?)?;
        let at = now_ms()?;
        let mut store = SessionRelayStore::open(&self.data_dir)?;
        match request {
            SessionRelayRequest::Begin { manifest } => {
                ensure!(
                    manifest.header.sender_peer == remote,
                    "wrong session sender"
                );
                verify_manifest(&manifest, &provider, at)?;
                Ok(serde_json::to_value(store.begin(
                    &manifest,
                    at,
                    config.storage_limit_bytes,
                    config.max_orders,
                )?)?)
            }
            SessionRelayRequest::Put {
                object_id,
                offset,
                bytes,
            } => {
                ensure!(
                    bytes.len() <= 4 * RELAY_CHUNK_BYTES.div_ceil(3),
                    "session relay chunk too large"
                );
                let bytes = STANDARD.decode(bytes)?;
                Ok(serde_json::to_value(
                    store.put(&object_id, &remote, offset, &bytes, at)?,
                )?)
            }
            SessionRelayRequest::Manifest { object_id } => {
                let manifest = store.manifest(&object_id, &remote, at)?;
                verify_manifest(&manifest, &provider, at)?;
                Ok(serde_json::to_value(manifest)?)
            }
            SessionRelayRequest::Get { object_id, offset } => {
                let bytes = store.get(&object_id, &remote, offset, at)?;
                Ok(json!(STANDARD.encode(bytes)))
            }
            SessionRelayRequest::Acknowledge { object_id, receipt } => {
                let manifest = store.manifest(&object_id, &remote, at)?;
                verify_manifest(&manifest, &provider, at)?;
                receipt.verify_header(&manifest.header)?;
                store.put_receipt(&object_id, &remote, &receipt, at)?;
                Ok(Value::Null)
            }
            SessionRelayRequest::Receipt { object_id } => Ok(serde_json::to_value(
                store.receipt(&object_id, &remote, at)?,
            )?),
        }
    }

    /// Upload an already sealed object, reusing its committed offset on retry.
    pub async fn upload_session_object(
        &self,
        node: &Runtime,
        object: &EncryptedSessionObject,
        capsule: &[u8; CAPSULE_BYTES],
    ) -> Result<()> {
        self.ensure_mode(Mode::Charity)?;
        verify_object(object)?;
        ensure!(
            object.header.sender_peer == parse_word(&self.identity.device_id()?)?,
            "wrong session sender"
        );
        ensure!(
            object.ciphertext.get(32..64) == Some(hash(capsule).as_slice()),
            "wrong session capsule"
        );
        let relay = &object.header.provider_peer;
        let object_id = object.header.object_id;
        let manifest = RelayManifest {
            header: object.header.clone(),
            signature: object.signature.context("missing session signature")?,
            capsule: STANDARD.encode(capsule),
        };
        let total = object.header.payload_bytes;
        let mut progress: RelayProgress = request(
            node,
            relay,
            SessionRelayRequest::Begin {
                manifest: Box::new(manifest),
            },
        )
        .await?;
        loop {
            ensure!(
                progress.committed <= total
                    && progress.complete == (progress.committed == total)
                    && (progress.complete || progress.committed % RELAY_CHUNK_BYTES as u64 == 0),
                "invalid session relay progress"
            );
            if progress.complete {
                return Ok(());
            }
            let start = progress.committed as usize;
            let end = (start + RELAY_CHUNK_BYTES).min(total as usize);
            progress = request(
                node,
                relay,
                SessionRelayRequest::Put {
                    object_id,
                    offset: start as u64,
                    bytes: STANDARD.encode(&object.ciphertext[start..end]),
                },
            )
            .await?;
            ensure!(
                progress.committed == end as u64,
                "session relay committed the wrong extent"
            );
        }
    }

    /// Fetch and verify the signed ciphertext and its separate encrypted key package.
    pub async fn download_session_object(
        &self,
        node: &Runtime,
        relay: Word,
        object_id: Word,
    ) -> Result<(EncryptedSessionObject, [u8; CAPSULE_BYTES])> {
        self.ensure_mode(Mode::Charity)?;
        let manifest: RelayManifest =
            request(node, &relay, SessionRelayRequest::Manifest { object_id }).await?;
        let capsule = verify_manifest(&manifest, &relay, now_ms()?)?;
        ensure!(
            manifest.header.object_id == object_id
                && manifest.header.receiver_peer == parse_word(&self.identity.device_id()?)?,
            "session relay returned another object's manifest"
        );
        let size = manifest.header.payload_bytes as usize;
        let mut ciphertext = Vec::with_capacity(size);
        while ciphertext.len() < size {
            let offset = ciphertext.len();
            let encoded: String = request(
                node,
                &relay,
                SessionRelayRequest::Get {
                    object_id,
                    offset: offset as u64,
                },
            )
            .await?;
            let bytes = STANDARD.decode(encoded)?;
            ensure!(
                bytes.len() == (size - offset).min(RELAY_CHUNK_BYTES),
                "invalid session relay chunk"
            );
            ciphertext.extend_from_slice(&bytes);
        }
        let object = EncryptedSessionObject {
            header: manifest.header,
            ciphertext,
            signature: Some(manifest.signature),
        };
        verify_object(&object)?;
        ensure!(
            object.ciphertext.get(32..64) == Some(hash(&capsule).as_slice()),
            "wrong session capsule"
        );
        Ok((object, capsule))
    }

    pub async fn acknowledge_session_object(
        &self,
        node: &Runtime,
        object: &EncryptedSessionObject,
        receipt: SessionStoredReceipt,
    ) -> Result<()> {
        self.ensure_mode(Mode::Charity)?;
        receipt.verify(object)?;
        request(
            node,
            &object.header.provider_peer,
            SessionRelayRequest::Acknowledge {
                object_id: object.header.object_id,
                receipt,
            },
        )
        .await
    }

    pub async fn session_object_receipt(
        &self,
        node: &Runtime,
        object: &EncryptedSessionObject,
    ) -> Result<Option<SessionStoredReceipt>> {
        self.ensure_mode(Mode::Charity)?;
        let receipt: Option<SessionStoredReceipt> = request(
            node,
            &object.header.provider_peer,
            SessionRelayRequest::Receipt {
                object_id: object.header.object_id,
            },
        )
        .await?;
        if let Some(receipt) = &receipt {
            receipt.verify(object)?;
        }
        Ok(receipt)
    }

    pub(super) async fn purge_session_relay(&self) -> Result<()> {
        if !self
            .data_dir
            .join("delivery/session-relay.sqlite3")
            .exists()
        {
            return Ok(());
        }
        let _relay = self.session_relay_operations.lock().await;
        SessionRelayStore::open(&self.data_dir)?.purge(now_ms()?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        config::{self, Config},
        crypto::LocalIdentity,
        model::{EventBody, IngestStatus, Sender},
        store::Store,
    };
    use std::{path::Path, sync::Arc};

    async fn node(dir: &Path, identity: &LocalIdentity, relay: bool) -> Result<Arc<Runtime>> {
        config::private_dir(dir)?;
        let bind = "127.0.0.1:0".parse()?;
        let mut config = Config::local(bind, vec![], false);
        config.web_enabled = false;
        config.network.bind = Some(bind);
        config.save(dir)?;
        config::private_write(&dir.join("data-epoch.json"), &config::data_epoch_marker())?;
        config::private_write(&dir.join("identity.json"), &serde_json::to_vec(identity)?)?;
        let node = Arc::new(Runtime::open(dir).await?);
        node.delivery
            .configure_local(super::super::DeliveryConfig {
                enabled: true,
                session_relay_enabled: relay,
                ..Default::default()
            })?;
        node.start().await?;
        Ok(node)
    }

    #[tokio::test]
    async fn session_relay_resumes_over_service_and_returns_the_recipient_receipt() -> Result<()> {
        let dir = std::env::temp_dir().join(format!("session-relay-{}", uuid::Uuid::new_v4()));
        config::private_dir(&dir)?;
        let sender = LocalIdentity::generate();
        let receiver = LocalIdentity::generate();
        let provider = LocalIdentity::generate();
        let sender_id = sender.device_id()?;
        let receiver_id = receiver.device_id()?;
        let receiver_peer = parse_word(&receiver_id)?;
        let relay_peer = parse_word(&provider.device_id()?)?;
        let source = node(&dir.join("sender"), &sender, false).await?;
        let target = node(&dir.join("receiver"), &receiver, false).await?;
        let relay_dir = dir.join("relay");
        let relay = node(&relay_dir, &provider, true).await?;
        source.net()?.learn(&target.net()?.addr()).await;
        source.net()?.learn(&relay.net()?.addr()).await;
        target.net()?.learn(&relay.net()?.addr()).await;

        let network = sender.fixture_network_id()?;
        let state = sender.sign_network_fixture(1, vec![sender_id.clone(), receiver_id.clone()])?;
        let mut sent_events = Store::open(":memory:")?;
        let mut received_events = Store::open(":memory:")?;
        for store in [&mut sent_events, &mut received_events] {
            store.install_trust_bundle(&network, &state)?;
        }
        sent_events.create_conversation(
            &sender,
            &network,
            "source",
            "Source",
            sender.owner_fixture(&network, "source", &sender_id)?,
            1,
        )?;
        sent_events.create_conversation(
            &receiver,
            &network,
            "target",
            "Target",
            sender.owner_fixture(&network, "target", &receiver_id)?,
            2,
        )?;
        for peer in [sender_id, receiver_id] {
            received_events
                .ingest_events(&network, &sent_events.events_from(&network, &peer, 1, 16)?)?;
        }
        let signed = sent_events.append_event(
            &sender,
            &network,
            "target",
            EventBody::BotMessaged {
                message_id: "relay-test".into(),
                from: Sender {
                    conversation_id: "source".into(),
                    name: "Source".into(),
                },
                text: format!("test private relay marker {}", "\"".repeat(3500)),
                hops: 1,
            },
            vec![],
            3,
        )?;
        let at = now_ms()?.saturating_sub(1000);
        let (object, capsule) = source
            .delivery
            .seal_session_event(
                &source,
                &network,
                &signed,
                receiver_peer,
                relay_peer,
                at..(at + 60_000),
            )
            .await?;
        let object_id = object.header.object_id;
        assert!(object.header.chunk_count > 1);
        let manifest = RelayManifest {
            header: object.header.clone(),
            signature: object.signature.context("missing fixture signature")?,
            capsule: STANDARD.encode(capsule),
        };
        let begin = || SessionRelayRequest::Begin {
            manifest: Box::new(manifest.clone()),
        };
        assert!(
            request::<RelayProgress>(&target, &relay_peer, begin())
                .await
                .is_err()
        );
        assert_eq!(
            request::<RelayProgress>(&source, &relay_peer, begin())
                .await?
                .committed,
            0
        );
        let first = STANDARD.encode(&object.ciphertext[..RELAY_CHUNK_BYTES]);
        let put = || SessionRelayRequest::Put {
            object_id,
            offset: 0,
            bytes: first.clone(),
        };
        assert!(
            request::<RelayProgress>(&target, &relay_peer, put())
                .await
                .is_err()
        );
        assert_eq!(
            request::<RelayProgress>(&source, &relay_peer, put())
                .await?
                .committed,
            RELAY_CHUNK_BYTES as u64
        );
        assert!(
            target
                .delivery
                .download_session_object(&target, relay_peer, object_id)
                .await
                .is_err()
        );
        relay.shutdown().await;
        drop(relay);

        let relay = node(&relay_dir, &provider, true).await?;
        source.net()?.learn(&relay.net()?.addr()).await;
        target.net()?.learn(&relay.net()?.addr()).await;
        source.net()?.peer_restarted(&provider.device_id()?);
        target.net()?.peer_restarted(&provider.device_id()?);
        assert_eq!(
            request::<RelayProgress>(&source, &relay_peer, begin())
                .await?
                .committed,
            RELAY_CHUNK_BYTES as u64
        );
        source
            .delivery
            .upload_session_object(&source, &object, &capsule)
            .await?;
        assert!(
            source
                .delivery
                .download_session_object(&source, relay_peer, object_id)
                .await
                .is_err()
        );

        let (downloaded, received_capsule) = target
            .delivery
            .download_session_object(&target, relay_peer, object_id)
            .await?;
        assert_eq!(downloaded, object);
        let ingested = target.delivery.ingest_session_event(
            &mut received_events,
            &network,
            &downloaded,
            &received_capsule,
            now_ms()?,
        )?;
        assert_eq!(ingested.sync.statuses, [IngestStatus::Accepted]);
        assert_eq!(received_events.event(&network, &signed.hash)?, Some(signed));
        let receipt = ingested.stored_receipt.context("missing storage receipt")?;
        for _ in 0..2 {
            target
                .delivery
                .acknowledge_session_object(&target, &object, receipt.clone())
                .await?;
        }
        assert_eq!(
            source
                .delivery
                .session_object_receipt(&source, &object)
                .await?,
            Some(receipt)
        );
        assert!(!relay_dir.join("delivery-wallet.json").exists());
        assert!(!relay_dir.join("delivery/service.sqlite3").exists());
        source.shutdown().await;
        target.shutdown().await;
        relay.shutdown().await;
        drop((source, target, relay));
        std::fs::remove_dir_all(dir)?;
        Ok(())
    }
}
