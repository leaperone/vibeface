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
    CAPSULE_BYTES, EncryptedSessionObject, MAX_SESSION_OBJECT_BYTES, SESSION_AEAD_TAG_BYTES, Word,
    hash, verify_peer,
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
    Inbox {
        sender: Word,
        conversation: String,
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
            SessionRelayRequest::Inbox {
                sender,
                conversation,
            } => {
                vibeface_protocol::validate_id(&conversation).map_err(anyhow::Error::msg)?;
                Ok(serde_json::to_value(store.inbox(
                    &remote,
                    &sender,
                    &conversation,
                    at,
                )?)?)
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

    pub(super) async fn session_inbox(
        &self,
        node: &Runtime,
        route: &super::SessionEndpointRoute,
    ) -> Result<Vec<Word>> {
        self.ensure_mode(Mode::Charity)?;
        let ids: Vec<Word> = request(
            node,
            &parse_word(&route.relay)?,
            SessionRelayRequest::Inbox {
                sender: parse_word(&route.peer)?,
                conversation: route.conversation_id.clone(),
            },
        )
        .await?;
        ensure!(ids.len() <= 16, "session relay returned an oversized inbox");
        Ok(ids)
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
                    && (progress.complete
                        || progress.committed.is_multiple_of(RELAY_CHUNK_BYTES as u64)),
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
        self.download_session_object_limited(node, relay, object_id, MAX_SESSION_OBJECT_BYTES)
            .await
    }

    pub(super) async fn download_session_object_limited(
        &self,
        node: &Runtime,
        relay: Word,
        object_id: Word,
        limit: usize,
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
        ensure!(size <= limit, "session object exceeds its expected extent");
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
        delivery::session::SessionRoute,
        model::{Attachment, EventBody, IngestStatus, Sender},
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
        let mut sent_events = Store::open(dir.join("sender/session-test.sqlite3"))?;
        let receiver_events = dir.join("receiver/session-test.sqlite3");
        let mut received_events = Store::open(&receiver_events)?;
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
        let bot = sender.fixture_admin().attest(
            &network,
            "source",
            EventBody::BotCreated {
                bot_id: "bot".into(),
                name: "File reader".into(),
                instructions: "Read instructions from the attachment".into(),
                project: None,
                attestation: None,
                cause: None,
            },
        )?;
        sent_events.append_event(&sender, &network, "source", bot, vec![], 2)?;
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
        let content = b"private session attachment".repeat(1300);
        let attachment = Attachment {
            hash: crate::crypto::digest(&content),
            size: content.len() as u64,
            name: "private-file.txt".into(),
        };
        let blob = attachment.blob();
        sent_events.put_blob(&blob, &content)?;
        let input = sender.fixture_admin().attest(
            &network,
            "source",
            EventBody::InputPosted {
                message_id: "relay-file".into(),
                target: crate::workspace::InputTarget::BotMain {
                    bot_id: "bot".into(),
                },
                intent: crate::workspace::InputIntent::Continue,
                sequence: 1,
                expected_revision: None,
                text: "Read the attached file".into(),
                attachments: vec![attachment],
                report_results: Some(false),
                attestation: None,
            },
        )?;
        let file_event =
            sent_events.append_event(&sender, &network, "source", input, vec![blob.clone()], 4)?;
        let (file_message, file_message_capsule) = source
            .delivery
            .seal_session_event(
                &source,
                &network,
                &file_event,
                receiver_peer,
                relay_peer,
                at..(at + 60_000),
            )
            .await?;
        source
            .delivery
            .upload_session_object(&source, &file_message, &file_message_capsule)
            .await?;
        let (file_message, file_message_capsule) = target
            .delivery
            .download_session_object(&target, relay_peer, file_message.header.object_id)
            .await?;
        let missing = target.delivery.ingest_session_event(
            &mut received_events,
            &network,
            &file_message,
            &file_message_capsule,
            now_ms()?,
        )?;
        assert_eq!(missing.sync.statuses, [IngestStatus::Accepted]);
        assert!(missing.stored_receipt.is_none());
        assert!(!received_events.has_blob(&blob)?);
        let route = SessionRoute {
            network: network.clone(),
            recipient: receiver_peer,
            relay: relay_peer,
            validity: at..(at + 60_000),
        };
        let parts = source
            .delivery
            .seal_session_attachment(&source, &route, &file_event, &blob, &content)
            .await?;
        let mut downloaded_parts = Vec::new();
        for (object, capsule) in &parts {
            assert!(object.header.chunk_count > 1);
            assert!(
                !object
                    .ciphertext
                    .windows(b"private session attachment".len())
                    .any(|window| window == b"private session attachment")
            );
            source
                .delivery
                .upload_session_object(&source, object, capsule)
                .await?;
            downloaded_parts.push(
                target
                    .delivery
                    .download_session_object(&target, relay_peer, object.header.object_id)
                    .await?,
            );
        }
        assert!(
            target
                .delivery
                .ingest_session_attachment(
                    &mut received_events,
                    &network,
                    &file_event.hash,
                    &blob,
                    &[],
                    now_ms()?,
                )
                .is_err()
        );
        assert!(!received_events.has_blob(&blob)?);
        let acknowledgments = target.delivery.ingest_session_attachment(
            &mut received_events,
            &network,
            &file_event.hash,
            &blob,
            &downloaded_parts,
            now_ms()?,
        )?;
        drop(received_events);
        let mut received_events = Store::open(&receiver_events)?;
        assert_eq!(
            received_events.read_blob(&network, "source", &blob.hash)?.1,
            content
        );
        for ((object, _), receipt) in downloaded_parts.iter().zip(acknowledgments) {
            target
                .delivery
                .acknowledge_session_object(&target, object, receipt.clone())
                .await?;
            assert_eq!(
                source
                    .delivery
                    .session_object_receipt(&source, object)
                    .await?,
                Some(receipt)
            );
        }
        assert!(
            target
                .delivery
                .ingest_session_event(
                    &mut received_events,
                    &network,
                    &file_message,
                    &file_message_capsule,
                    now_ms()?,
                )?
                .stored_receipt
                .is_some()
        );
        assert!(!relay_dir.join("delivery-wallet.json").exists());
        assert!(!relay_dir.join("delivery/service.sqlite3").exists());
        source.shutdown().await;
        target.shutdown().await;
        relay.shutdown().await;
        drop((source, target, relay, sent_events, received_events));
        std::fs::remove_dir_all(dir)?;
        Ok(())
    }
    #[tokio::test]
    async fn configured_endpoints_automatically_relay_new_events_from_the_local_log() -> Result<()>
    {
        let dir = std::env::temp_dir().join(format!("session-auto-{}", uuid::Uuid::new_v4()));
        config::private_dir(&dir)?;
        let sender = LocalIdentity::generate();
        let receiver = LocalIdentity::generate();
        let provider = LocalIdentity::generate();
        let source = node(&dir.join("sender"), &sender, false).await?;
        let target = node(&dir.join("receiver"), &receiver, false).await?;
        let relay = node(&dir.join("relay"), &provider, true).await?;
        source.net()?.learn(&target.net()?.addr()).await;
        source.net()?.learn(&relay.net()?.addr()).await;
        target.net()?.learn(&relay.net()?.addr()).await;

        let sender_id = sender.device_id()?;
        let receiver_id = receiver.device_id()?;
        let provider_id = provider.device_id()?;
        let network = sender.fixture_network_id()?;
        let state = sender.sign_network_fixture(1, vec![sender_id.clone(), receiver_id.clone()])?;
        let source_seed = (sender.clone(), network.clone(), state.clone());
        let prelude = source
            .store
            .call("session_test.prelude", move |store| {
                let (identity, network, state) = source_seed;
                let peer = identity.device_id()?;
                store.install_trust_bundle(&network, &state)?;
                store.create_conversation(
                    &identity,
                    &network,
                    "auto",
                    "Automatic",
                    identity.owner_fixture(&network, "auto", &peer)?,
                    1,
                )?;
                store.events_from(&network, &peer, 1, 16)
            })
            .await?;
        let seeded = network.clone();
        target
            .store
            .call("session_test.recipient_prelude", move |store| {
                store.install_trust_bundle(&seeded, &state)?;
                store.ingest_events(&seeded, &prelude)?;
                Ok(())
            })
            .await?;

        let outgoing = super::super::SessionEndpointRoute {
            conversation_id: "auto".into(),
            peer: receiver_id,
            relay: provider_id.clone(),
        };
        let incoming = super::super::SessionEndpointRoute {
            conversation_id: "auto".into(),
            peer: sender_id.clone(),
            relay: provider_id,
        };
        assert!(!dir.join("sender/delivery/session-outbox.sqlite3").exists());
        let mut source_settings = source.delivery.config();
        source_settings.session_route = Some(outgoing.clone());
        source.delivery.configure_local(source_settings)?;
        let route_id = hex::encode(hash(&serde_json::to_vec(&(
            &network, &sender_id, outgoing,
        ))?));
        tokio::time::timeout(std::time::Duration::from_secs(10), async {
            loop {
                let path = dir.join("sender");
                if path.join("delivery/session-outbox.sqlite3").exists() {
                    let outbox =
                        super::super::session_outbox_store::SessionOutboxStore::open(&path)?;
                    if outbox.cursor(&route_id)?.is_some() {
                        return Ok::<(), anyhow::Error>(());
                    }
                }
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
        })
        .await
        .context("automatic sender did not initialize")??;

        let content = b"only the recipient opens this file".repeat(300);
        let message_content = content.clone();
        let event_network = network.clone();
        let signed = source
            .store
            .call("session_test.write", move |store| {
                let bot = sender.fixture_admin().attest(
                    &event_network,
                    "auto",
                    EventBody::BotCreated {
                        bot_id: "auto-bot".into(),
                        name: "Automatic".into(),
                        instructions: "Handle files".into(),
                        project: None,
                        attestation: None,
                        cause: None,
                    },
                )?;
                let settings =
                    store.append_event(&sender, &event_network, "auto", bot, vec![], 2)?;
                let attachment = Attachment {
                    hash: crate::crypto::digest(&message_content),
                    size: message_content.len() as u64,
                    name: "read.txt".into(),
                };
                let blob = attachment.blob();
                store.put_blob(&blob, &message_content)?;
                let input = sender.fixture_admin().attest(
                    &event_network,
                    "auto",
                    EventBody::InputPosted {
                        message_id: "auto-message".into(),
                        target: crate::workspace::InputTarget::BotMain {
                            bot_id: "auto-bot".into(),
                        },
                        intent: crate::workspace::InputIntent::Continue,
                        sequence: 1,
                        expected_revision: None,
                        text: "Read my file".into(),
                        attachments: vec![attachment],
                        report_results: Some(false),
                        attestation: None,
                    },
                )?;
                let message = store.append_event(
                    &sender,
                    &event_network,
                    "auto",
                    input,
                    vec![blob.clone()],
                    3,
                )?;
                Ok((settings, message, blob))
            })
            .await?;
        let relay_database = dir.join("relay/delivery/session-relay.sqlite3");
        let mut target_settings = target.delivery.config();
        target_settings.session_route = Some(incoming.clone());
        target.delivery.configure_local(target_settings)?;
        tokio::time::timeout(std::time::Duration::from_secs(15), async {
            loop {
                let (network, (settings, message, blob)) = (network.clone(), signed.clone());
                let present = target
                    .store
                    .read("session_test.verify", move |store| {
                        Ok(store.event(&network, &settings.hash)? == Some(settings)
                            && store.event(&network, &message.hash)? == Some(message)
                            && store.has_blob(&blob)?)
                    })
                    .await?;
                let acknowledged = if relay_database.exists() {
                    let db = rusqlite::Connection::open_with_flags(
                        &relay_database,
                        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
                    )?;
                    db.busy_timeout(std::time::Duration::from_secs(2))?;
                    db.query_row(
                        "SELECT count(*) FROM objects WHERE receipt IS NOT NULL",
                        [],
                        |row| row.get::<_, u32>(0),
                    )?
                } else {
                    0
                };
                if present
                    && acknowledged >= 2
                    && target
                        .delivery
                        .session_inbox(&target, &incoming)
                        .await?
                        .is_empty()
                {
                    return Ok::<(), anyhow::Error>(());
                }
                tokio::time::sleep(std::time::Duration::from_millis(30)).await;
            }
        })
        .await
        .context("automatic receiver did not ingest and acknowledge the configured session")??;
        let read_network = network.clone();
        let blob = signed.2.hash;
        let (_, bytes) = target
            .store
            .read("session_test.file", move |store| {
                store.read_blob(&read_network, "auto", &blob)
            })
            .await?;
        assert_eq!(bytes, content);
        assert!(
            target
                .store
                .read("session_test.no_implicit_scheduler", |store| {
                    Ok(store.scheduler_jobs("auto")?.is_empty())
                })
                .await?
        );
        source.shutdown().await;
        target.shutdown().await;
        relay.shutdown().await;
        drop((source, target, relay));
        std::fs::remove_dir_all(dir)?;
        Ok(())
    }
}
