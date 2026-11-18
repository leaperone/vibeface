use alloc::{string::String, vec::Vec};

use chacha20poly1305::{
    ChaCha20Poly1305, Nonce,
    aead::{Aead, KeyInit, Payload},
};
use serde::{Deserialize, Serialize};

use crate::{Error, PeerSignature, Result, Word, hash, verify_peer};

/// Payload classes supported by the session relay. Secret credentials are
/// intentionally absent from this enum and cannot enter an envelope.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[repr(u8)]
pub enum SessionObjectKind {
    Message = 1,
    Attachment = 2,
    BotSettings = 3,
    ConversationState = 4,
}

impl TryFrom<u8> for SessionObjectKind {
    type Error = Error;

    fn try_from(value: u8) -> Result<Self> {
        match value {
            1 => Ok(Self::Message),
            2 => Ok(Self::Attachment),
            3 => Ok(Self::BotSettings),
            4 => Ok(Self::ConversationState),
            _ => Err(Error("unknown session object kind")),
        }
    }
}

pub const SESSION_OBJECT_VERSION: u8 = 1;
pub const MAX_SESSION_ID_BYTES: usize = 256;
pub const MAX_SESSION_OBJECT_BYTES: usize = 16 * 1024 * 1024;
pub const MAX_SESSION_CHUNK_BYTES: usize = 64 * 1024;
pub const SESSION_AEAD_TAG_BYTES: usize = 16;

/// Encrypts one opaque session payload with ChaCha20-Poly1305.
///
/// The caller owns nonce uniqueness. This helper rejects an all-zero key or nonce as an
/// accidental empty value, but it does not generate, persist, or negotiate either value.
pub fn encrypt_session_payload(
    key: &Word,
    nonce: [u8; 12],
    aad: &[u8],
    plaintext: &[u8],
) -> Result<Vec<u8>> {
    validate_aead_inputs(key, &nonce, plaintext.len())?;
    let cipher = ChaCha20Poly1305::new_from_slice(key).map_err(|_| Error("session AEAD key"))?;
    cipher
        .encrypt(
            Nonce::from_slice(&nonce),
            Payload {
                msg: plaintext,
                aad,
            },
        )
        .map_err(|_| Error("session AEAD encryption"))
}

/// Decrypts one opaque session payload. The caller must supply the same AAD and a unique nonce
/// that belongs to this object; this function does not perform key exchange or nonce allocation.
pub fn decrypt_session_payload(
    key: &Word,
    nonce: [u8; 12],
    aad: &[u8],
    ciphertext: &[u8],
) -> Result<Vec<u8>> {
    if ciphertext.len() < SESSION_AEAD_TAG_BYTES {
        return Err(Error("session AEAD ciphertext too short"));
    }
    validate_aead_inputs(key, &nonce, ciphertext.len() - SESSION_AEAD_TAG_BYTES)?;
    let cipher = ChaCha20Poly1305::new_from_slice(key).map_err(|_| Error("session AEAD key"))?;
    cipher
        .decrypt(
            Nonce::from_slice(&nonce),
            Payload {
                msg: ciphertext,
                aad,
            },
        )
        .map_err(|_| Error("session AEAD authentication"))
}

fn validate_aead_inputs(key: &Word, nonce: &[u8; 12], payload_bytes: usize) -> Result<()> {
    if key.iter().all(|byte| *byte == 0) {
        return Err(Error("session AEAD key is empty"));
    }
    if nonce.iter().all(|byte| *byte == 0) {
        return Err(Error("session AEAD nonce is empty"));
    }
    if payload_bytes == 0 || payload_bytes > MAX_SESSION_OBJECT_BYTES {
        return Err(Error("session AEAD payload size"));
    }
    Ok(())
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionObjectHeader {
    pub version: u8,
    pub kind: SessionObjectKind,
    pub object_id: Word,
    pub session_id: String,
    pub conversation_id: String,
    pub sequence: u64,
    pub previous_object_id: Word,
    pub sender_peer: Word,
    pub receiver_peer: Word,
    pub provider_peer: Word,
    pub payload_hash: Word,
    pub payload_bytes: u64,
    pub created_at_ms: u64,
    pub expires_at_ms: u64,
    pub chunk_count: u32,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EncryptedSessionObject {
    pub header: SessionObjectHeader,
    /// The relay treats this as opaque ciphertext. It never parses this byte
    /// vector or interprets it according to `kind`.
    pub ciphertext: Vec<u8>,
    /// Signature over the unsigned signing bytes. It is excluded from object identity.
    pub signature: Option<PeerSignature>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionObjectChunk {
    pub object_id: Word,
    pub index: u32,
    pub offset: u64,
    pub bytes: Vec<u8>,
    pub hash: Word,
}

impl SessionObjectHeader {
    fn validate_fields(&self) -> Result<()> {
        if self.version != SESSION_OBJECT_VERSION {
            return Err(Error("unsupported session object version"));
        }
        if self.session_id.is_empty()
            || self.session_id.len() > MAX_SESSION_ID_BYTES
            || self.conversation_id.is_empty()
            || self.conversation_id.len() > MAX_SESSION_ID_BYTES
        {
            return Err(Error("invalid session object scope"));
        }
        if self.sequence == 0 {
            if self.previous_object_id != [0; 32] {
                return Err(Error("first session object has a previous object"));
            }
        } else if self.previous_object_id == [0; 32] {
            return Err(Error("session object sequence has no previous object"));
        }
        for peer in [self.sender_peer, self.receiver_peer, self.provider_peer] {
            if peer == [0; 32] {
                return Err(Error("session object has a zero peer"));
            }
        }
        if self.payload_bytes == 0 || self.payload_bytes as usize > MAX_SESSION_OBJECT_BYTES {
            return Err(Error("session object payload size"));
        }
        if self.payload_hash == [0; 32] || self.chunk_count == 0 {
            return Err(Error("session object payload commitment"));
        }
        if self.expires_at_ms <= self.created_at_ms {
            return Err(Error("session object expiry"));
        }
        Ok(())
    }

    fn identity_bytes(&self) -> Result<Vec<u8>> {
        self.validate_fields()?;
        let mut copy = self.clone();
        copy.object_id = [0; 32];
        copy.canonical_bytes_unchecked()
    }

    fn canonical_bytes_unchecked(&self) -> Result<Vec<u8>> {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"vibeface-session-object-v1");
        bytes.push(self.version);
        bytes.push(self.kind as u8);
        bytes.extend_from_slice(&self.object_id);
        put_string(&mut bytes, &self.session_id)?;
        put_string(&mut bytes, &self.conversation_id)?;
        bytes.extend_from_slice(&self.sequence.to_be_bytes());
        bytes.extend_from_slice(&self.previous_object_id);
        bytes.extend_from_slice(&self.sender_peer);
        bytes.extend_from_slice(&self.receiver_peer);
        bytes.extend_from_slice(&self.provider_peer);
        bytes.extend_from_slice(&self.payload_hash);
        bytes.extend_from_slice(&self.payload_bytes.to_be_bytes());
        bytes.extend_from_slice(&self.created_at_ms.to_be_bytes());
        bytes.extend_from_slice(&self.expires_at_ms.to_be_bytes());
        bytes.extend_from_slice(&self.chunk_count.to_be_bytes());
        Ok(bytes)
    }

    pub fn canonical_bytes(&self) -> Result<Vec<u8>> {
        self.validate_fields()?;
        if self.object_id != hash(&self.identity_bytes()?) {
            return Err(Error("session object identity hash"));
        }
        self.canonical_bytes_unchecked()
    }

    pub fn signing_bytes(&self) -> Result<Vec<u8>> {
        self.canonical_bytes()
    }

    pub fn signing_digest(&self) -> Result<Word> {
        Ok(hash(&self.signing_bytes()?))
    }
}

impl EncryptedSessionObject {
    pub fn new(
        kind: SessionObjectKind,
        session_id: String,
        conversation_id: String,
        sequence: u64,
        previous_object_id: Word,
        sender_peer: Word,
        receiver_peer: Word,
        provider_peer: Word,
        created_at_ms: u64,
        expires_at_ms: u64,
        ciphertext: Vec<u8>,
        chunk_bytes: usize,
    ) -> Result<Self> {
        if chunk_bytes == 0 || chunk_bytes > MAX_SESSION_CHUNK_BYTES {
            return Err(Error("invalid session chunk size"));
        }
        if ciphertext.is_empty() || ciphertext.len() > MAX_SESSION_OBJECT_BYTES {
            return Err(Error("invalid session ciphertext size"));
        }
        let payload_hash = hash(&ciphertext);
        let chunk_count = ciphertext.len().div_ceil(chunk_bytes) as u32;
        let mut header = SessionObjectHeader {
            version: SESSION_OBJECT_VERSION,
            kind,
            object_id: [0; 32],
            session_id,
            conversation_id,
            sequence,
            previous_object_id,
            sender_peer,
            receiver_peer,
            provider_peer,
            payload_hash,
            payload_bytes: ciphertext.len() as u64,
            created_at_ms,
            expires_at_ms,
            chunk_count,
        };
        header.object_id = hash(&header.identity_bytes()?);
        let object = Self {
            header,
            ciphertext,
            signature: None,
        };
        object.validate(chunk_bytes)?;
        Ok(object)
    }

    pub fn validate(&self, chunk_bytes: usize) -> Result<()> {
        if chunk_bytes == 0 || chunk_bytes > MAX_SESSION_CHUNK_BYTES {
            return Err(Error("invalid session chunk size"));
        }
        self.header.validate_fields()?;
        if self.header.payload_bytes != self.ciphertext.len() as u64
            || self.header.payload_hash != hash(&self.ciphertext)
            || self.header.chunk_count != self.ciphertext.len().div_ceil(chunk_bytes) as u32
        {
            return Err(Error("session ciphertext commitment"));
        }
        self.header.canonical_bytes()?;
        Ok(())
    }

    pub fn chunks(&self, chunk_bytes: usize) -> Result<Vec<SessionObjectChunk>> {
        self.validate(chunk_bytes)?;
        let mut result = Vec::with_capacity(self.header.chunk_count as usize);
        for (index, bytes) in self.ciphertext.chunks(chunk_bytes).enumerate() {
            result.push(SessionObjectChunk {
                object_id: self.header.object_id,
                index: index as u32,
                offset: (index * chunk_bytes) as u64,
                bytes: bytes.to_vec(),
                hash: hash(bytes),
            });
        }
        Ok(result)
    }

    pub fn from_chunks(
        header: SessionObjectHeader,
        chunks: &[SessionObjectChunk],
        chunk_bytes: usize,
    ) -> Result<Self> {
        if chunk_bytes == 0 || chunk_bytes > MAX_SESSION_CHUNK_BYTES {
            return Err(Error("invalid session chunk size"));
        }
        header.validate_fields()?;
        if chunks.len() != header.chunk_count as usize {
            return Err(Error("session chunk count"));
        }
        let mut ordered: Vec<Option<&SessionObjectChunk>> =
            (0..header.chunk_count).map(|_| None).collect();
        for chunk in chunks {
            if chunk.object_id != header.object_id
                || chunk.index >= header.chunk_count
                || chunk.bytes.is_empty()
                || chunk.bytes.len() > chunk_bytes
                || chunk.hash != hash(&chunk.bytes)
            {
                return Err(Error("invalid session chunk"));
            }
            if ordered[chunk.index as usize].is_some() {
                return Err(Error("duplicate session chunk"));
            }
            ordered[chunk.index as usize] = Some(chunk);
        }
        let mut ciphertext = Vec::with_capacity(header.payload_bytes as usize);
        for (index, chunk) in ordered.into_iter().enumerate() {
            let chunk = chunk.ok_or(Error("missing session chunk"))?;
            if chunk.offset != (index * chunk_bytes) as u64 {
                return Err(Error("session chunk offset"));
            }
            ciphertext.extend_from_slice(&chunk.bytes);
        }
        let object = Self {
            header,
            ciphertext,
            signature: None,
        };
        object.validate(chunk_bytes)?;
        Ok(object)
    }

    pub fn signing_bytes(&self, chunk_bytes: usize) -> Result<Vec<u8>> {
        self.validate(chunk_bytes)?;
        let mut bytes = self.header.signing_bytes()?;
        bytes.extend_from_slice(&self.ciphertext);
        Ok(bytes)
    }

    pub fn signing_digest(&self, chunk_bytes: usize) -> Result<Word> {
        Ok(hash(&self.signing_bytes(chunk_bytes)?))
    }

    pub fn with_signature(mut self, signature: PeerSignature) -> Self {
        self.signature = Some(signature);
        self
    }

    pub fn verify_signature(
        &self,
        sender_peer: &Word,
        domain: &str,
        chunk_bytes: usize,
    ) -> Result<()> {
        let signature = self
            .signature
            .as_ref()
            .ok_or(Error("session object signature missing"))?;
        verify_peer(
            sender_peer,
            domain,
            &self.signing_bytes(chunk_bytes)?,
            signature,
        )
    }

    pub fn same_object(&self, other: &Self) -> bool {
        self.header.object_id == other.header.object_id
    }
}

fn put_string(output: &mut Vec<u8>, value: &str) -> Result<()> {
    if value.len() > MAX_SESSION_ID_BYTES {
        return Err(Error("session object string too long"));
    }
    output.extend_from_slice(&(value.len() as u32).to_be_bytes());
    output.extend_from_slice(value.as_bytes());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::{Signer, SigningKey};

    fn object(kind: SessionObjectKind) -> EncryptedSessionObject {
        EncryptedSessionObject::new(
            kind,
            "network-1".into(),
            "conversation-1".into(),
            0,
            [0; 32],
            [1; 32],
            [2; 32],
            [3; 32],
            10,
            20,
            b"opaque encrypted session payload".to_vec(),
            8,
        )
        .unwrap()
    }

    #[test]
    fn all_supported_kinds_have_no_secret_variant() {
        for (raw, kind) in [
            (1, SessionObjectKind::Message),
            (2, SessionObjectKind::Attachment),
            (3, SessionObjectKind::BotSettings),
            (4, SessionObjectKind::ConversationState),
        ] {
            assert_eq!(SessionObjectKind::try_from(raw).unwrap(), kind);
        }
        assert!(SessionObjectKind::try_from(5).is_err());
    }

    #[test]
    fn canonical_identity_and_signing_digest_are_stable() {
        let object = object(SessionObjectKind::Message);
        let restored = EncryptedSessionObject::new(
            SessionObjectKind::Message,
            "network-1".into(),
            "conversation-1".into(),
            0,
            [0; 32],
            [1; 32],
            [2; 32],
            [3; 32],
            10,
            20,
            b"opaque encrypted session payload".to_vec(),
            8,
        )
        .unwrap();
        assert_eq!(object.header.object_id, restored.header.object_id);
        assert_eq!(
            object.signing_digest(8).unwrap(),
            restored.signing_digest(8).unwrap()
        );
    }

    #[test]
    fn chunks_round_trip_and_retry_identity_are_deterministic() {
        let object = object(SessionObjectKind::Attachment);
        let chunks = object.chunks(8).unwrap();
        let restored =
            EncryptedSessionObject::from_chunks(object.header.clone(), &chunks, 8).unwrap();
        assert!(object.same_object(&restored));
        assert_eq!(restored.ciphertext, object.ciphertext);
    }

    #[test]
    fn chunks_reject_missing_duplicate_tampered_and_wrong_offset() {
        let object = object(SessionObjectKind::ConversationState);
        let chunks = object.chunks(8).unwrap();
        assert!(
            EncryptedSessionObject::from_chunks(
                object.header.clone(),
                &chunks[..chunks.len() - 1],
                8
            )
            .is_err()
        );

        let mut duplicate = chunks.clone();
        duplicate[1].index = duplicate[0].index;
        assert!(EncryptedSessionObject::from_chunks(object.header.clone(), &duplicate, 8).is_err());

        let mut tampered = chunks.clone();
        tampered[0].bytes[0] ^= 1;
        assert!(EncryptedSessionObject::from_chunks(object.header.clone(), &tampered, 8).is_err());

        let mut wrong_offset = chunks;
        wrong_offset[0].offset = 1;
        assert!(EncryptedSessionObject::from_chunks(object.header, &wrong_offset, 8).is_err());
    }

    #[test]
    fn tampering_with_header_or_previous_object_is_rejected() {
        let current = object(SessionObjectKind::BotSettings);
        let mut header = current.header.clone();
        header.payload_hash[0] ^= 1;
        assert!(header.canonical_bytes().is_err());

        let mut next = object(SessionObjectKind::Message).header;
        next.sequence = 1;
        assert!(next.canonical_bytes().is_err());
    }

    #[test]
    fn signature_is_required_and_does_not_change_object_identity() {
        let unsigned = object(SessionObjectKind::Message);
        let key = SigningKey::from_bytes(&[42; 32]);
        let peer = key.verifying_key().to_bytes();
        assert!(
            unsigned
                .verify_signature(&peer, "session-object", 8)
                .is_err()
        );
        let signature = key.sign(
            &crate::signing_message("session-object", &unsigned.signing_bytes(8).unwrap()).unwrap(),
        );
        let object_id = unsigned.header.object_id;
        let signed = unsigned.with_signature(PeerSignature::from_bytes(signature.to_bytes()));
        assert_eq!(signed.header.object_id, object_id);
        signed.verify_signature(&peer, "session-object", 8).unwrap();
        assert!(signed.verify_signature(&peer, "wrong-domain", 8).is_err());
    }

    #[test]
    fn session_payload_aead_round_trip_binds_aad() {
        let key = [7; 32];
        let nonce = [9; 12];
        let aad = b"object-header";
        let plaintext = b"opaque message and settings payload";
        let ciphertext = encrypt_session_payload(&key, nonce, aad, plaintext).unwrap();
        assert_eq!(ciphertext.len(), plaintext.len() + SESSION_AEAD_TAG_BYTES);
        assert_eq!(
            decrypt_session_payload(&key, nonce, aad, &ciphertext).unwrap(),
            plaintext
        );
    }

    #[test]
    fn session_payload_aead_rejects_wrong_key_aad_nonce_and_empty_values() {
        let key = [7; 32];
        let nonce = [9; 12];
        let ciphertext = encrypt_session_payload(&key, nonce, b"aad", b"payload").unwrap();
        assert!(decrypt_session_payload(&[8; 32], nonce, b"aad", &ciphertext).is_err());
        assert!(decrypt_session_payload(&key, nonce, b"other", &ciphertext).is_err());
        assert!(decrypt_session_payload(&key, [10; 12], b"aad", &ciphertext).is_err());
        assert!(encrypt_session_payload(&[0; 32], nonce, b"aad", b"payload").is_err());
        assert!(encrypt_session_payload(&key, [0; 12], b"aad", b"payload").is_err());
        assert!(encrypt_session_payload(&key, nonce, b"aad", b"").is_err());
    }

    #[test]
    fn session_payload_aead_rejects_oversized_input_and_short_ciphertext() {
        let key = [7; 32];
        let nonce = [9; 12];
        let oversized = alloc::vec![0u8; MAX_SESSION_OBJECT_BYTES + 1];
        assert!(encrypt_session_payload(&key, nonce, b"aad", &oversized).is_err());
        assert!(
            decrypt_session_payload(&key, nonce, b"aad", &[0; SESSION_AEAD_TAG_BYTES - 1]).is_err()
        );
    }
}
