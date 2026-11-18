//! Node-side boundary for relaying original signed Bot session events.

pub(crate) use super::charity::CharityKey;
use super::{Delivery, session_relay_store::RELAY_CHUNK_BYTES, types::Mode};
use crate::{
    crypto::{LocalIdentity, check_event, verify_event_signature},
    model::{Event, EventBody, IngestStatus, SignedEvent},
    runtime::Runtime,
    store::{Ingested, Store},
};
use anyhow::{Context, Result, bail, ensure};
use base64::{Engine, engine::general_purpose::STANDARD};
use rand::{RngCore, rngs::OsRng};
use rusqlite::{OptionalExtension, params};
use std::path::{Path, PathBuf};
use vibeface_delivery::{
    CAPSULE_BYTES, EncryptedSessionObject, MAX_SESSION_OBJECT_BYTES, PeerSignature, Secret32,
    SessionObjectKind, Word, decrypt_session_payload, encrypt_session_payload, hash,
    open_session_key, seal_session_key,
};
use zeroize::Zeroizing;

pub(super) const SIGNING_DOMAIN: &str = "bot-session-object";
const RECEIPT_DOMAIN: &str = "bot-session-event-stored";
const AEAD_DOMAIN: &[u8] = b"vibeface:bot-session-event:v1";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionIdempotency {
    New,
    AlreadySeen,
}

const OBJECT_FILE_LIMIT: u64 = (MAX_SESSION_OBJECT_BYTES as u64 * 2) + 1024 * 1024;

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredSessionObject {
    header: vibeface_delivery::SessionObjectHeader,
    signature: Option<vibeface_delivery::PeerSignature>,
    ciphertext_base64: String,
}

/// Private storage for verified opaque ciphertext and its envelope metadata.
pub struct SessionObjectStore {
    root: PathBuf,
}

impl SessionObjectStore {
    pub fn open(data_dir: &Path) -> Result<Self> {
        let root = data_dir.join("delivery").join("session-objects");
        crate::config::private_dir(&root)?;
        Ok(Self { root })
    }

    fn path(&self, object_id: &Word) -> PathBuf {
        self.root.join(format!("{}.bin", hex::encode(object_id)))
    }

    pub fn put_verified(&self, object: &EncryptedSessionObject) -> Result<SessionIdempotency> {
        verify_object(object)?;
        let stored = StoredSessionObject {
            header: object.header.clone(),
            signature: object.signature,
            ciphertext_base64: STANDARD.encode(&object.ciphertext),
        };
        let bytes = serde_json::to_vec(&stored)?;
        let path = self.path(&object.header.object_id);
        if path.exists() {
            let existing = self.read(&path)?;
            anyhow::ensure!(
                existing.header == object.header
                    && existing.ciphertext == object.ciphertext
                    && existing.signature == object.signature,
                "session object id conflicts with stored ciphertext"
            );
            return Ok(SessionIdempotency::AlreadySeen);
        }
        vibeface_bridge::platform::privatefs::write_atomic(&path, &bytes)?;
        Ok(SessionIdempotency::New)
    }

    pub fn get(&self, object_id: &Word) -> Result<Option<EncryptedSessionObject>> {
        let path = self.path(object_id);
        match std::fs::symlink_metadata(&path) {
            Ok(_) => Ok(Some(self.read(&path)?)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error.into()),
        }
    }

    fn read(&self, path: &Path) -> Result<EncryptedSessionObject> {
        let bytes = vibeface_bridge::platform::privatefs::read_private_bounded(
            path,
            OBJECT_FILE_LIMIT,
            "session object",
        )?;
        let stored: StoredSessionObject = serde_json::from_slice(&bytes)?;
        let ciphertext = STANDARD.decode(stored.ciphertext_base64)?;
        let object = EncryptedSessionObject {
            header: stored.header,
            ciphertext,
            signature: stored.signature,
        };
        verify_object(&object)?;
        anyhow::ensure!(
            self.path(&object.header.object_id) == path,
            "session object path does not match its object id"
        );
        Ok(object)
    }
}

/// Classify the event kinds that may cross an encrypted session relay.
pub fn kind_for_event(event: &EventBody) -> Result<SessionObjectKind> {
    match event {
        EventBody::MessagePosted { .. }
        | EventBody::InputPosted { .. }
        | EventBody::BotMessaged { .. } => Ok(SessionObjectKind::Message),
        EventBody::BotCreated { .. }
        | EventBody::BotChanged { .. }
        | EventBody::BotAvatarSet { .. }
        | EventBody::BotProjectSet { .. }
        | EventBody::BotDeleted { .. }
        | EventBody::ScheduleControlled { .. }
        | EventBody::MemoryControlled { .. } => Ok(SessionObjectKind::BotSettings),
        EventBody::ConversationOwner { .. }
        | EventBody::SessionAttributed { .. }
        | EventBody::SessionMuted { .. }
        | EventBody::ScheduleRunClaimed { .. }
        | EventBody::ContextCheckpoint { .. } => Ok(SessionObjectKind::ConversationState),
        _ => bail!("event kind is not supported by the encrypted session relay"),
    }
}

fn checked_event(network: &str, signed: &SignedEvent) -> Result<Event> {
    let event = check_event(network, signed)?;
    ensure!(
        !crate::logging::blank_credentials_in(&mut serde_json::to_value(&event.body)?),
        "session event contains a credential"
    );
    Ok(event)
}

pub(super) fn verify_object(object: &EncryptedSessionObject) -> Result<()> {
    ensure!(
        object.ciphertext.len() > 64 + 12 + vibeface_delivery::SESSION_AEAD_TAG_BYTES,
        "session ciphertext is missing its key commitment or sealed body"
    );
    Ok(object.verify_signature(
        &object.header.sender_peer,
        SIGNING_DOMAIN,
        RELAY_CHUNK_BYTES,
    )?)
}

fn capsule_context(receiver: &Word, key_id: &Word) -> Word {
    hash(&[receiver.as_slice(), key_id.as_slice()].concat())
}

impl Delivery {
    /// Obtain the signed receiving key over the authenticated service channel.
    pub async fn seal_session_event(
        &self,
        node: &Runtime,
        network: &str,
        signed: &SignedEvent,
        recipient: Word,
        relay: Word,
        validity: std::ops::Range<u64>,
    ) -> Result<(EncryptedSessionObject, [u8; CAPSULE_BYTES])> {
        self.ensure_mode(Mode::Charity)?;
        let key = Self::session_receiving_key(node, &hex::encode(recipient)).await?;
        seal_event(
            &self.identity,
            network,
            signed,
            recipient,
            relay,
            &key,
            validity,
        )
    }

    /// Open a released capsule using the recipient's local key history.
    pub fn ingest_session_event(
        &self,
        store: &mut Store,
        network: &str,
        object: &EncryptedSessionObject,
        capsule: &[u8; CAPSULE_BYTES],
        now_ms: u64,
    ) -> Result<SessionIngested> {
        self.ensure_mode(Mode::Charity)?;
        let header = &object.header;
        verify_object(object)?;
        ensure!(
            hex::encode(header.receiver_peer) == self.identity.device_id()?,
            "session recipient mismatch"
        );
        let key_id = object
            .ciphertext
            .first_chunk()
            .context("session key identifier is missing")?;
        let secret = self.session_receiving_secret(
            &header.receiver_peer,
            key_id,
            header.created_at_ms..header.expires_at_ms,
        )?;
        ingest_event(
            store,
            &self.identity,
            network,
            object,
            &secret,
            capsule,
            now_ms,
        )
    }
}

/// Returns the signed ciphertext and a separate HPKE capsule for the recipient.
pub(crate) fn seal_event(
    sender: &LocalIdentity,
    network: &str,
    signed: &SignedEvent,
    recipient: Word,
    relay: Word,
    receiving_key: &CharityKey,
    validity: std::ops::Range<u64>,
) -> Result<(EncryptedSessionObject, [u8; CAPSULE_BYTES])> {
    let event = checked_event(network, signed)?;
    ensure!(
        event.author == sender.device_id()?,
        "session sender is not the event author"
    );
    verify_event_signature(signed, &event.author)?;
    let (key_id, public_key) = receiving_key.session_material(&recipient, validity.clone())?;
    let key = Secret32::new(super::random_word())?;
    let seed = Secret32::new(super::random_word())?;
    let capsule = seal_session_key(
        &public_key,
        &capsule_context(&recipient, &key_id),
        &key,
        &seed,
    )?;
    let mut nonce = [0; 12];
    OsRng.fill_bytes(&mut nonce);
    let plaintext = Zeroizing::new(serde_json::to_vec(signed)?);
    let mut ciphertext = key_id.to_vec();
    ciphertext.extend_from_slice(&hash(&capsule));
    ciphertext.extend_from_slice(&nonce);
    ciphertext.extend(encrypt_session_payload(
        key.as_bytes(),
        nonce,
        AEAD_DOMAIN,
        &plaintext,
    )?);
    let object = EncryptedSessionObject::new(
        kind_for_event(&event.body)?,
        network.into(),
        event.scope_id,
        0,
        [0; 32],
        hex::decode(event.author)?
            .try_into()
            .ok()
            .context("invalid sender peer")?,
        recipient,
        relay,
        validity.start,
        validity.end,
        ciphertext,
        RELAY_CHUNK_BYTES,
    )?;
    let mut signature = [0; 64];
    hex::decode_to_slice(
        sender.sign_payload(SIGNING_DOMAIN, &object.signing_bytes(RELAY_CHUNK_BYTES)?)?,
        &mut signature,
    )?;
    Ok((
        object.with_signature(PeerSignature::from_bytes(signature)),
        capsule,
    ))
}

/// The recipient attests durable event storage. This says nothing about Bot execution or payment.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionStoredReceipt {
    object_id: Word,
    signature: String,
}

impl SessionStoredReceipt {
    pub fn verify(&self, object: &EncryptedSessionObject) -> Result<()> {
        verify_object(object)?;
        self.verify_header(&object.header)
    }

    pub(super) fn verify_header(
        &self,
        header: &vibeface_delivery::SessionObjectHeader,
    ) -> Result<()> {
        ensure!(
            header.object_id == self.object_id,
            "receipt identifies another session object"
        );
        crate::crypto::verify_payload(
            &hex::encode(header.receiver_peer),
            RECEIPT_DOMAIN,
            &self.object_id,
            &self.signature,
        )
    }
}

pub struct SessionIngested {
    pub sync: Ingested,
    pub stored_receipt: Option<SessionStoredReceipt>,
}

/// Pass the decrypted, original signed event to the ordinary sync authorization and DAG.
pub fn ingest_event(
    store: &mut Store,
    receiver: &LocalIdentity,
    network: &str,
    object: &EncryptedSessionObject,
    secret: &Secret32,
    capsule: &[u8; CAPSULE_BYTES],
    now_ms: u64,
) -> Result<SessionIngested> {
    let header = &object.header;
    let recipient = receiver.device_id()?;
    ensure!(
        header.session_id == network,
        "session object belongs to another network"
    );
    ensure!(
        hex::encode(header.receiver_peer) == recipient,
        "session recipient mismatch"
    );
    ensure!(
        header.created_at_ms <= now_ms && now_ms < header.expires_at_ms,
        "session object is outside its validity interval"
    );
    verify_object(object)?;
    let sender = hex::encode(header.sender_peer);
    store.authorize_node(network, &recipient)?;
    store.authorize_node(network, &sender)?;
    let (key_header, ciphertext) = object
        .ciphertext
        .split_at_checked(64)
        .context("session key commitment is missing")?;
    let (key_id, capsule_hash) = key_header.split_at(32);
    ensure!(
        capsule_hash == hash(capsule),
        "session key capsule commitment mismatch"
    );
    let key = open_session_key(
        secret,
        &capsule_context(&header.receiver_peer, key_id.try_into()?),
        capsule,
    )?;
    let (nonce, ciphertext) = ciphertext
        .split_at_checked(12)
        .context("session nonce is missing")?;
    let plaintext = Zeroizing::new(decrypt_session_payload(
        key.as_bytes(),
        nonce.try_into()?,
        AEAD_DOMAIN,
        ciphertext,
    )?);
    let signed: SignedEvent = serde_json::from_slice(&plaintext)?;
    let event = checked_event(network, &signed)?;
    ensure!(
        event.author == sender
            && event.scope_id == header.conversation_id
            && kind_for_event(&event.body)? == header.kind,
        "signed event does not match the session object"
    );
    verify_event_signature(&signed, &sender)?;
    let mut ingested = store.ingest_events(network, std::slice::from_ref(&signed))?;
    let mut stored_receipt = None;
    if matches!(
        ingested.statuses[0],
        IngestStatus::Accepted | IngestStatus::Duplicate
    ) {
        // Normal sync also reports Duplicate for pending events and retained fork evidence.
        let effective: Option<bool> = store
            .connection
            .query_row(
                "SELECT effective FROM events WHERE network_id=? AND hash=?",
                params![network, signed.hash],
                |row| row.get(0),
            )
            .optional()?;
        match effective {
            None => ingested.statuses[0] = IngestStatus::Pending,
            Some(false) => ingested.statuses[0] = IngestStatus::Conflict,
            Some(true) => {
                for blob in &event.blobs {
                    if !store.has_blob(blob)? {
                        return Ok(SessionIngested {
                            sync: ingested,
                            stored_receipt,
                        });
                    }
                    // Sync can use size as a readiness hint. Receipts require content integrity.
                    store.read_blob(network, &event.scope_id, &blob.hash)?;
                }
                stored_receipt = Some(SessionStoredReceipt {
                    object_id: header.object_id,
                    signature: receiver.sign_payload(RECEIPT_DOMAIN, &header.object_id)?,
                });
            }
        }
    }
    Ok(SessionIngested {
        sync: ingested,
        stored_receipt,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::WIRE_VERSION;

    fn signed_message_object() -> EncryptedSessionObject {
        let sender = LocalIdentity::generate();
        let network = "01".repeat(32);
        let signed = sender
            .sign_event(Event {
                version: WIRE_VERSION,
                network_id: network.clone(),
                author: sender.device_id().unwrap(),
                seq: 1,
                prev: None,
                deps: vec![],
                scope_id: "conversation-1".into(),
                created_at_ms: 100,
                body: EventBody::BotMessaged {
                    message_id: "message-1".into(),
                    from: crate::model::Sender {
                        conversation_id: "other".into(),
                        name: "source".into(),
                    },
                    text: "encrypted request".into(),
                    hops: 1,
                },
                blobs: vec![],
            })
            .unwrap();
        let receiver = LocalIdentity::generate();
        let recipient = hex::decode(receiver.device_id().unwrap())
            .unwrap()
            .try_into()
            .unwrap();
        let secret = Secret32::new([4; 32]).unwrap();
        let receiving_key = CharityKey::issue(&receiver, &secret, 1, 0, 1).unwrap();
        seal_event(
            &sender,
            &network,
            &signed,
            recipient,
            [3; 32],
            &receiving_key,
            100..200,
        )
        .unwrap()
        .0
    }

    fn test_dir(label: &str) -> std::path::PathBuf {
        let path = std::env::temp_dir().join(format!(
            "vibeface-session-receipt-{label}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&path).unwrap();
        path
    }

    #[test]
    fn session_key_history_survives_restart_without_enabling_wallets() -> Result<()> {
        let directory = test_dir("key-restart");
        let identity = LocalIdentity::generate();
        let recipient: Word = hex::decode(identity.device_id()?)?
            .try_into()
            .ok()
            .context("bad recipient fixture")?;
        let now = super::super::now();
        let interval = now * 1000..(now + 60) * 1000;
        let delivery = Delivery::open(&directory, identity.clone())?;
        assert!(delivery.charity_key().is_err());
        drop(delivery);
        let config = super::super::DeliveryConfig {
            enabled: true,
            ..Default::default()
        };
        vibeface_bridge::platform::privatefs::write_atomic(
            &directory.join("delivery-settings.json"),
            &serde_json::to_vec(&config)?,
        )?;
        let delivery = Delivery::open(&directory, identity.clone())?;
        let key = delivery.charity_key()?;
        let (key_id, public_key) = key.session_material(&recipient, interval.clone())?;
        let context = capsule_context(&recipient, &key_id);
        let object_key = Secret32::new([9; 32])?;
        let capsule =
            seal_session_key(&public_key, &context, &object_key, &Secret32::new([8; 32])?)?;
        drop(delivery);

        let restarted = Delivery::open(&directory, identity)?;
        let secret = restarted.session_receiving_secret(&recipient, &key_id, interval.clone())?;
        let opened = open_session_key(&secret, &context, &capsule)?;
        assert_eq!(opened.as_bytes(), object_key.as_bytes());
        assert!(key.session_material(&[2; 32], interval.clone()).is_err());
        assert!(
            key.session_material(&recipient, interval.start..u64::MAX)
                .is_err()
        );
        assert!(!directory.join("delivery-wallet.json").exists());
        drop(restarted);
        std::fs::remove_dir_all(directory)?;
        Ok(())
    }

    #[test]
    fn object_store_persists_opaque_ciphertext_and_is_idempotent() {
        let dir = test_dir("objects");
        let object = signed_message_object();
        let store = SessionObjectStore::open(&dir).unwrap();
        assert_eq!(
            store.put_verified(&object).unwrap(),
            SessionIdempotency::New
        );
        assert_eq!(
            store.put_verified(&object).unwrap(),
            SessionIdempotency::AlreadySeen
        );
        let restored = store.get(&object.header.object_id).unwrap().unwrap();
        assert_eq!(restored.ciphertext, object.ciphertext);
        assert_eq!(restored.header, object.header);
        drop(store);
        let reopened = SessionObjectStore::open(&dir).unwrap();
        assert!(reopened.get(&object.header.object_id).unwrap().is_some());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn object_store_rejects_tampered_or_symlinked_files() {
        let dir = test_dir("object-errors");
        let object = signed_message_object();
        let store = SessionObjectStore::open(&dir).unwrap();
        store.put_verified(&object).unwrap();
        let path = store.path(&object.header.object_id);
        let bytes = std::fs::read(&path).unwrap();
        let mut tampered = bytes.clone();
        tampered[0] = b'!';
        std::fs::write(&path, tampered).unwrap();
        assert!(store.get(&object.header.object_id).is_err());
        std::fs::write(&path, bytes).unwrap();
        #[cfg(unix)]
        {
            let target = dir.join("outside.bin");
            std::fs::write(&target, std::fs::read(&path).unwrap()).unwrap();
            std::fs::remove_file(&path).unwrap();
            std::os::unix::fs::symlink(&target, &path).unwrap();
            assert!(store.get(&object.header.object_id).is_err());
        }
        std::fs::remove_dir_all(dir).unwrap();
    }
}
