//! Authenticated Noise channels shared by native nodes and browser clients.

use ed25519_dalek::{Signature, Signer, SigningKey, VerifyingKey};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
#[cfg(target_arch = "wasm32")]
use wasm_bindgen::prelude::*;
use zeroize::Zeroizing;

pub mod membership;

mod recovery;
pub use recovery::{recovery_open, recovery_seal};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Error(String);

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Error {}

impl From<&str> for Error {
    fn from(message: &str) -> Self {
        Self(message.into())
    }
}

impl From<String> for Error {
    fn from(message: String) -> Self {
        Self(message)
    }
}

impl From<anyhow::Error> for Error {
    fn from(error: anyhow::Error) -> Self {
        Self(format!("{error:#}"))
    }
}

impl From<serde_json::Error> for Error {
    fn from(error: serde_json::Error) -> Self {
        Self(error.to_string())
    }
}

#[cfg(target_arch = "wasm32")]
impl From<Error> for JsValue {
    fn from(error: Error) -> Self {
        JsValue::from_str(&error.0)
    }
}

type Result<T> = std::result::Result<T, Error>;
const NOISE: &str = "Noise_XX_25519_ChaChaPoly_SHA256";
const MAX_MESSAGE: usize = 65_535;
const MAX_CONTEXT: usize = 4_096;
pub const MAX_APPLICATION_MESSAGE: usize = 16 * 1024 * 1024;
const RECORD_PLAINTEXT: usize = 60 * 1024;
const STATIC_DOMAIN: &str = "noise-static";

#[cfg_attr(target_arch = "wasm32", wasm_bindgen)]
pub struct Identity {
    key: SigningKey,
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen)]
impl Identity {
    pub fn generate() -> Result<Identity> {
        let mut seed = Zeroizing::new([0u8; 32]);
        getrandom::fill(seed.as_mut()).map_err(|_| "secure randomness unavailable")?;
        Ok(Self {
            key: SigningKey::from_bytes(&seed),
        })
    }

    #[cfg_attr(target_arch = "wasm32", wasm_bindgen(js_name = fromSecretHex))]
    pub fn from_secret_hex(secret: &str) -> Result<Identity> {
        let seed = Zeroizing::new(decode32(secret)?);
        Ok(Self {
            key: SigningKey::from_bytes(&seed),
        })
    }

    #[cfg_attr(target_arch = "wasm32", wasm_bindgen(js_name = secretHex))]
    pub fn secret_hex(&self) -> String {
        hex::encode(Zeroizing::new(self.key.to_bytes()).as_ref())
    }

    #[cfg_attr(target_arch = "wasm32", wasm_bindgen(js_name = publicKeyHex))]
    pub fn public_key_hex(&self) -> String {
        hex::encode(self.key.verifying_key().as_bytes())
    }

    #[cfg_attr(target_arch = "wasm32", wasm_bindgen(js_name = peerId))]
    pub fn peer_id(&self) -> String {
        hex::encode(Sha256::digest(self.key.verifying_key().as_bytes()))
    }

    pub fn sign(&self, domain: &str, payload: &[u8]) -> Result<String> {
        Ok(hex::encode(
            self.key.sign(&signing_message(domain, payload)?).to_bytes(),
        ))
    }
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen)]
pub fn verify(public_key: &str, domain: &str, payload: &[u8], signature: &str) -> Result<()> {
    let key = VerifyingKey::from_bytes(&decode32(public_key)?)
        .map_err(|_| "invalid identity public key")?;
    let bytes = hex::decode(signature).map_err(|_| "invalid signature encoding")?;
    let signature = Signature::from_slice(&bytes).map_err(|_| "invalid signature size")?;
    key.verify_strict(&signing_message(domain, payload)?, &signature)
        .map_err(|_| "invalid identity signature".into())
}

fn decode32(value: &str) -> Result<[u8; 32]> {
    let mut bytes = [0u8; 32];
    hex::decode_to_slice(value, &mut bytes).map_err(|_| "expected a 32-byte hexadecimal key")?;
    Ok(bytes)
}

fn signing_message(domain: &str, payload: &[u8]) -> Result<Vec<u8>> {
    if domain.is_empty()
        || domain.len() > 64
        || !domain
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
    {
        return Err("invalid signing domain".into());
    }
    Ok(format!(
        "vibeface:message:v1:{domain}:{}",
        hex::encode(Sha256::digest(payload))
    )
    .into_bytes())
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Proof {
    public_key: String,
    signature: String,
    payload: Vec<u8>,
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen)]
pub struct Handshake {
    inner: Option<snow::HandshakeState>,
    public_key: String,
    signature: String,
    expected_peer_id: Option<String>,
    remote_public_key: Option<String>,
    first_write: bool,
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen)]
impl Handshake {
    /// An unpinned responder still requires application membership authorization.
    #[cfg_attr(target_arch = "wasm32", wasm_bindgen(constructor))]
    pub fn new(
        initiator: bool,
        secret_hex: &str,
        expected_peer_id: Option<String>,
        context: &[u8],
    ) -> Result<Handshake> {
        if context.len() > MAX_CONTEXT {
            return Err("Noise context too large".into());
        }
        if let Some(expected) = &expected_peer_id {
            decode32(expected)?;
        }
        let identity = Identity::from_secret_hex(secret_hex)?;
        let builder = snow::Builder::new(NOISE.parse().map_err(|_| "invalid Noise protocol")?);
        let keys = builder
            .generate_keypair()
            .map_err(|_| "Noise key generation failed")?;
        let private = Zeroizing::new(keys.private);
        let signature = identity.sign(STATIC_DOMAIN, &keys.public)?;
        let builder = builder
            .local_private_key(&private)
            .map_err(|_| "invalid Noise static key")?
            .prologue(context)
            .map_err(|_| "invalid Noise context")?;
        let inner = if initiator {
            builder.build_initiator()
        } else {
            builder.build_responder()
        }
        .map_err(|_| "Noise initialization failed")?;
        Ok(Self {
            inner: Some(inner),
            public_key: identity.public_key_hex(),
            signature,
            expected_peer_id,
            remote_public_key: None,
            first_write: initiator,
        })
    }

    /// Application data is accepted only after the remote identity is authenticated.
    pub fn write(&mut self, payload: &[u8]) -> Result<Vec<u8>> {
        let result = self.write_inner(payload);
        if result.is_err() {
            self.inner = None;
            self.remote_public_key = None;
        }
        result
    }

    pub fn read(&mut self, message: &[u8]) -> Result<Vec<u8>> {
        let result = self.read_inner(message);
        if result.is_err() {
            self.inner = None;
            self.remote_public_key = None;
        }
        result
    }

    #[cfg_attr(target_arch = "wasm32", wasm_bindgen(js_name = remotePublicKey))]
    pub fn remote_public_key(&self) -> Option<String> {
        self.remote_public_key.clone()
    }

    #[cfg_attr(target_arch = "wasm32", wasm_bindgen(js_name = remoteId))]
    pub fn remote_id(&self) -> Option<String> {
        self.remote_public_key.as_ref().map(|key| {
            hex::encode(Sha256::digest(
                hex::decode(key).expect("validated identity"),
            ))
        })
    }

    #[cfg_attr(target_arch = "wasm32", wasm_bindgen(js_name = isFinished))]
    pub fn is_finished(&self) -> bool {
        self.remote_public_key.is_some()
            && self
                .inner
                .as_ref()
                .is_some_and(snow::HandshakeState::is_handshake_finished)
    }

    pub fn finish(&mut self) -> Result<Channel> {
        if !self.is_finished() {
            return Err("Noise identity authentication incomplete".into());
        }
        let inner = self
            .inner
            .take()
            .ok_or("Noise handshake closed")?
            .into_transport_mode()
            .map_err(|_| "Noise transport transition failed")?;
        Ok(Channel {
            inner: Some(inner),
            incoming: Vec::new(),
            incoming_len: None,
        })
    }
}

impl Handshake {
    fn write_inner(&mut self, payload: &[u8]) -> Result<Vec<u8>> {
        if self.remote_public_key.is_none() && !payload.is_empty() {
            return Err(
                "Noise identity must be authenticated before sending application data".into(),
            );
        }
        let inner = self.inner.as_mut().ok_or("Noise handshake closed")?;
        let payload = if self.first_write {
            Vec::new()
        } else {
            serde_json::to_vec(&Proof {
                public_key: self.public_key.clone(),
                signature: self.signature.clone(),
                payload: payload.to_vec(),
            })
            .map_err(|_| "Noise identity payload encoding failed")?
        };
        let mut output = vec![0; MAX_MESSAGE];
        let size = inner
            .write_message(&payload, &mut output)
            .map_err(|_| "Noise handshake write failed")?;
        self.first_write = false;
        output.truncate(size);
        Ok(output)
    }

    fn read_inner(&mut self, message: &[u8]) -> Result<Vec<u8>> {
        if message.len() > MAX_MESSAGE {
            return Err("Noise handshake message too large".into());
        }
        let inner = self.inner.as_mut().ok_or("Noise handshake closed")?;
        let mut output = vec![0; MAX_MESSAGE];
        let size = inner
            .read_message(message, &mut output)
            .map_err(|_| "Noise handshake rejected")?;
        if size == 0 && inner.get_remote_static().is_none() {
            return Ok(Vec::new());
        }
        let proof: Proof =
            serde_json::from_slice(&output[..size]).map_err(|_| "invalid Noise identity proof")?;
        let remote_static = inner
            .get_remote_static()
            .ok_or("Noise static key missing")?;
        verify(
            &proof.public_key,
            STATIC_DOMAIN,
            remote_static,
            &proof.signature,
        )?;
        let id = hex::encode(Sha256::digest(decode32(&proof.public_key)?));
        if self
            .expected_peer_id
            .as_ref()
            .is_some_and(|expected| expected != &id)
        {
            return Err("unexpected Noise peer identity".into());
        }
        self.remote_public_key = Some(proof.public_key);
        Ok(proof.payload)
    }
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen)]
pub struct Channel {
    inner: Option<snow::TransportState>,
    incoming: Vec<u8>,
    incoming_len: Option<usize>,
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen)]
impl Channel {
    pub fn seal(&mut self, plaintext: &[u8]) -> Result<Vec<u8>> {
        if plaintext.len() > MAX_MESSAGE - 16 {
            return Err("Noise plaintext too large".into());
        }
        let inner = self.inner.as_mut().ok_or("Noise channel closed")?;
        let mut output = vec![0; plaintext.len() + 16];
        let result = inner.write_message(plaintext, &mut output);
        match result {
            Ok(size) => {
                output.truncate(size);
                Ok(output)
            }
            Err(_) => {
                self.inner = None;
                Err("Noise encryption failed".into())
            }
        }
    }

    pub fn open(&mut self, ciphertext: &[u8]) -> Result<Vec<u8>> {
        if ciphertext.len() < 16 || ciphertext.len() > MAX_MESSAGE {
            self.inner = None;
            return Err("invalid Noise ciphertext size".into());
        }
        let inner = self.inner.as_mut().ok_or("Noise channel closed")?;
        let mut output = vec![0; ciphertext.len() - 16];
        match inner.read_message(ciphertext, &mut output) {
            Ok(size) => {
                output.truncate(size);
                Ok(output)
            }
            Err(_) => {
                self.inner = None;
                Err("Noise ciphertext authentication failed".into())
            }
        }
    }
}

impl Channel {
    pub fn seal_message(&mut self, payload: &[u8]) -> Result<Vec<Vec<u8>>> {
        if payload.len() > MAX_APPLICATION_MESSAGE {
            return Err("application message too large".into());
        }
        let mut frame = Vec::with_capacity(payload.len() + 4);
        frame.extend_from_slice(&(payload.len() as u32).to_be_bytes());
        frame.extend_from_slice(payload);
        frame
            .chunks(RECORD_PLAINTEXT)
            .map(|record| self.seal(record))
            .collect()
    }

    pub fn open_record(&mut self, record: &[u8]) -> Result<Option<Vec<u8>>> {
        let result = self.open_record_inner(record);
        if result.is_err() {
            self.inner = None;
            self.incoming.clear();
            self.incoming_len = None;
        }
        result
    }

    fn open_record_inner(&mut self, record: &[u8]) -> Result<Option<Vec<u8>>> {
        let plain = self.open(record)?;
        if plain.is_empty() || plain.len() > RECORD_PLAINTEXT {
            return Err("invalid application record size".into());
        }
        let payload = if self.incoming_len.is_none() {
            let prefix: [u8; 4] = plain
                .get(..4)
                .ok_or("incomplete application length")?
                .try_into()
                .expect("four bytes");
            let length = u32::from_be_bytes(prefix) as usize;
            if length > MAX_APPLICATION_MESSAGE {
                return Err("application message too large".into());
            }
            self.incoming_len = Some(length);
            &plain[4..]
        } else {
            plain.as_slice()
        };
        let expected = self.incoming_len.expect("validated application length");
        if payload.len() > expected - self.incoming.len() {
            return Err("application message has trailing bytes".into());
        }
        self.incoming.extend_from_slice(payload);
        if self.incoming.len() == expected {
            self.incoming_len = None;
            Ok(Some(std::mem::take(&mut self.incoming)))
        } else {
            Ok(None)
        }
    }
}

#[cfg(target_arch = "wasm32")]
#[wasm_bindgen]
impl Channel {
    #[wasm_bindgen(js_name = sealMessage)]
    pub fn seal_message_js(&mut self, payload: &[u8]) -> Result<js_sys::Array> {
        let records = self.seal_message(payload)?;
        Ok(records
            .iter()
            .map(|record| JsValue::from(js_sys::Uint8Array::from(record.as_slice())))
            .collect())
    }

    #[wasm_bindgen(js_name = openRecord)]
    pub fn open_record_js(&mut self, record: &[u8]) -> Result<JsValue> {
        Ok(match self.open_record(record)? {
            Some(payload) => js_sys::Uint8Array::from(payload.as_slice()).into(),
            None => JsValue::NULL,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn endpoints() -> (Identity, Identity, Handshake, Handshake) {
        let a = Identity::generate().unwrap();
        let b = Identity::generate().unwrap();
        let ah = Handshake::new(
            true,
            &a.secret_hex(),
            Some(b.peer_id()),
            b"vibeface:channel:v1",
        )
        .unwrap();
        let bh = Handshake::new(
            false,
            &b.secret_hex(),
            Some(a.peer_id()),
            b"vibeface:channel:v1",
        )
        .unwrap();
        (a, b, ah, bh)
    }

    fn connected() -> (Channel, Channel) {
        let (_, _, mut a, mut b) = endpoints();
        assert!(b.read(&a.write(&[]).unwrap()).unwrap().is_empty());
        assert!(a.read(&b.write(&[]).unwrap()).unwrap().is_empty());
        assert_eq!(
            b.read(&a.write(b"client-private").unwrap()).unwrap(),
            b"client-private"
        );
        (a.finish().unwrap(), b.finish().unwrap())
    }

    #[test]
    fn authenticated_private_payloads_and_ordered_transport() {
        let (mut a, mut b) = connected();
        let cipher = a.seal(b"sensitive request").unwrap();
        assert!(!cipher.windows(17).any(|part| part == b"sensitive request"));
        assert_eq!(b.open(&cipher).unwrap(), b"sensitive request");
        assert_eq!(a.open(&b.seal(b"response").unwrap()).unwrap(), b"response");
        assert!(b.open(&cipher).is_err());
        assert!(b.open(&a.seal(b"after replay").unwrap()).is_err());
    }

    #[test]
    fn wrong_expected_identity_fails_closed() {
        let (_, _, mut a, mut b) = endpoints();
        a.expected_peer_id = Some("ab".repeat(32));
        b.read(&a.write(&[]).unwrap()).unwrap();
        assert!(a.read(&b.write(&[]).unwrap()).is_err());
        assert!(a.remote_id().is_none());
        assert!(a.finish().is_err());
        assert!(a.write(&[]).is_err());
    }

    #[test]
    fn substituted_identity_signature_fails_closed() {
        let (_, _, mut a, mut b) = endpoints();
        b.signature = "00".repeat(64);
        b.read(&a.write(&[]).unwrap()).unwrap();
        assert!(a.read(&b.write(&[]).unwrap()).is_err());
        assert!(a.finish().is_err());
    }

    #[test]
    fn tampered_transport_fails_closed() {
        let (mut a, mut b) = connected();
        let mut cipher = a.seal(b"private").unwrap();
        cipher[0] ^= 1;
        assert!(b.open(&cipher).is_err());
        assert!(b.open(&a.seal(b"retry").unwrap()).is_err());
    }

    #[test]
    fn private_payload_cannot_leak_before_encryption() {
        let (_, _, mut a, _) = endpoints();
        assert!(a.finish().is_err());
        assert!(a.remote_id().is_none());
        assert!(a.write(b"private").is_err());
        assert!(a.write(&[]).is_err());
    }

    #[test]
    fn identity_signatures_are_domain_bound_and_restorable() {
        let a = Identity::generate().unwrap();
        let restored = Identity::from_secret_hex(&a.secret_hex()).unwrap();
        assert_eq!(a.peer_id(), restored.peer_id());
        let signature = a.sign("client-challenge", b"challenge").unwrap();
        verify(
            &a.public_key_hex(),
            "client-challenge",
            b"challenge",
            &signature,
        )
        .unwrap();
        assert!(verify(&a.public_key_hex(), "pair", b"challenge", &signature).is_err());
        assert!(a.sign("invalid:domain", b"data").is_err());
    }
    #[test]
    fn fragmented_application_messages_reassemble_and_reject_overflow() {
        let (mut a, mut b) = connected();
        for payload in [
            vec![],
            vec![0x5a; 150_000],
            vec![0x3c; MAX_APPLICATION_MESSAGE],
        ] {
            let records = a.seal_message(&payload).unwrap();
            for (index, record) in records.iter().enumerate() {
                let result = b.open_record(record).unwrap();
                if index == records.len() - 1 {
                    assert_eq!(result.unwrap(), payload);
                } else {
                    assert!(result.is_none());
                }
            }
        }
        assert!(
            a.seal_message(&vec![0; MAX_APPLICATION_MESSAGE + 1])
                .is_err()
        );
        let mut bad = (MAX_APPLICATION_MESSAGE as u32 + 1).to_be_bytes().to_vec();
        bad.push(1);
        assert!(b.open_record(&a.seal(&bad).unwrap()).is_err());
        assert!(b.open_record(&a.seal(&[0; 4]).unwrap()).is_err());
    }

    #[test]
    fn application_trailing_bytes_and_mismatched_prologue_fail() {
        let (mut a, mut b) = connected();
        assert!(b.open_record(&a.seal(&[0, 0, 0, 0, 1]).unwrap()).is_err());
        let (_, remote, mut initiator, _) = endpoints();
        let mut responder =
            Handshake::new(false, &remote.secret_hex(), None, b"another-service").unwrap();
        responder.read(&initiator.write(&[]).unwrap()).unwrap();
        assert!(initiator.read(&responder.write(&[]).unwrap()).is_err());
    }
    #[test]
    fn responder_cannot_send_application_data_before_authentication() {
        let (_, _, mut a, mut b) = endpoints();
        b.read(&a.write(&[]).unwrap()).unwrap();
        assert!(b.write(b"private server data").is_err());
        assert!(b.finish().is_err());
        assert!(b.remote_id().is_none());
    }

    #[test]
    fn write_failure_discards_authenticated_identity() {
        let (_, _, mut a, mut b) = endpoints();
        b.read(&a.write(&[]).unwrap()).unwrap();
        a.read(&b.write(&[]).unwrap()).unwrap();
        assert!(a.remote_id().is_some());
        a.write(&[]).unwrap();
        assert!(a.write(&[]).is_err());
        assert!(a.remote_id().is_none());
        assert!(a.finish().is_err());
    }
}
