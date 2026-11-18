//! Node-side boundary for relaying original signed Bot session events.

use crate::{
    crypto::{LocalIdentity, check_event, verify_event_signature},
    model::{Event, EventBody, SignedEvent},
    store::{Ingested, Store},
};
use anyhow::{Context, Result, bail, ensure};
use base64::{Engine, engine::general_purpose::STANDARD};
use rand::{RngCore, rngs::OsRng};
use std::path::{Path, PathBuf};
use vibeface_delivery::{
    EncryptedSessionObject, MAX_SESSION_CHUNK_BYTES, MAX_SESSION_OBJECT_BYTES, PeerSignature,
    Secret32, SessionObjectKind, Word, decrypt_session_payload, encrypt_session_payload,
};
use zeroize::Zeroizing;

const SIGNING_DOMAIN: &str = "bot-session-object";
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

fn verify_object(object: &EncryptedSessionObject) -> Result<()> {
    Ok(object.verify_signature(
        &object.header.sender_peer,
        SIGNING_DOMAIN,
        MAX_SESSION_CHUNK_BYTES,
    )?)
}

/// Encrypt the exact signed event. Key exchange is separate and the object has no key field.
pub fn seal_event(
    sender: &LocalIdentity,
    network: &str,
    signed: &SignedEvent,
    recipient: Word,
    relay: Word,
    key: &Secret32,
    validity: std::ops::Range<u64>,
) -> Result<EncryptedSessionObject> {
    let event = checked_event(network, signed)?;
    ensure!(
        event.author == sender.device_id()?,
        "session sender is not the event author"
    );
    verify_event_signature(signed, &event.author)?;
    let mut nonce = [0; 12];
    OsRng.fill_bytes(&mut nonce);
    let plaintext = Zeroizing::new(serde_json::to_vec(signed)?);
    let mut ciphertext = nonce.to_vec();
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
        MAX_SESSION_CHUNK_BYTES,
    )?;
    let mut signature = [0; 64];
    hex::decode_to_slice(
        sender.sign_payload(
            SIGNING_DOMAIN,
            &object.signing_bytes(MAX_SESSION_CHUNK_BYTES)?,
        )?,
        &mut signature,
    )?;
    Ok(object.with_signature(PeerSignature::from_bytes(signature)))
}

/// Pass the decrypted, original signed event to the ordinary sync authorization and DAG.
pub fn ingest_event(
    store: &mut Store,
    receiver: &LocalIdentity,
    network: &str,
    object: &EncryptedSessionObject,
    key: &Secret32,
    now_ms: u64,
) -> Result<Ingested> {
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
    let (nonce, ciphertext) = object
        .ciphertext
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
    store.ingest_events(network, &[signed])
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
        seal_event(
            &sender,
            &network,
            &signed,
            [2; 32],
            [3; 32],
            &Secret32::new([4; 32]).unwrap(),
            100..200,
        )
        .unwrap()
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
