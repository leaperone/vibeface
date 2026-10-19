//! Device signatures, membership and the browser-to-Node Noise channel.
//!
//! A `DeviceId` is the device's Ed25519 public key. Browsers keep that key in a
//! non-extractable WebCrypto `CryptoKey`, so this crate builds the exact bytes to
//! sign and verifies signatures, but never needs a browser's private key.

use ed25519_dalek::{Signature, Signer, SigningKey, VerifyingKey};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
#[cfg(target_arch = "wasm32")]
use wasm_bindgen::prelude::*;
use zeroize::Zeroizing;

pub mod join;
pub mod membership;

mod recovery;
pub use recovery::{recovery_open, recovery_seal};

type Result<T> = std::result::Result<T, String>;
const NOISE: &str = "Noise_XX_25519_ChaChaPoly_SHA256";
const MAX_MESSAGE: usize = 65_535;
const MAX_CONTEXT: usize = 4_096;
pub const MAX_APPLICATION_MESSAGE: usize = 16 * 1024 * 1024;
const RECORD_PLAINTEXT: usize = 60 * 1024;
const NOISE_STATIC_DOMAIN: &[u8] = b"vibeface:noise-static:v1";

/// A locally held Ed25519 key: a Node's device key or an unlocked recovery root.
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

    #[cfg_attr(target_arch = "wasm32", wasm_bindgen(js_name = deviceId))]
    pub fn device_id(&self) -> String {
        hex::encode(self.key.verifying_key().as_bytes())
    }

    pub fn sign(&self, domain: &str, payload: &[u8]) -> Result<String> {
        Ok(self.sign_message(&signing_message(domain, payload)?))
    }
}

impl Identity {
    pub fn sign_message(&self, message: &[u8]) -> String {
        hex::encode(self.key.sign(message).to_bytes())
    }
}

/// The exact bytes a device signs for `domain` over `payload`; browsers pass them to WebCrypto.
#[cfg_attr(target_arch = "wasm32", wasm_bindgen(js_name = signingMessage))]
pub fn signing_message(domain: &str, payload: &[u8]) -> Result<Vec<u8>> {
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

#[cfg_attr(target_arch = "wasm32", wasm_bindgen)]
pub fn verify(device_id: &str, domain: &str, payload: &[u8], signature: &str) -> Result<()> {
    verify_message(device_id, &signing_message(domain, payload)?, signature)
}

pub fn verify_message(device_id: &str, message: &[u8], signature: &str) -> Result<()> {
    let key =
        VerifyingKey::from_bytes(&decode32(device_id)?).map_err(|_| "invalid device public key")?;
    let bytes = hex::decode(signature).map_err(|_| "invalid signature encoding")?;
    let signature = Signature::from_slice(&bytes).map_err(|_| "invalid signature size")?;
    key.verify_strict(message, &signature)
        .map_err(|_| "invalid device signature".to_owned())
}

/// A `DeviceId` is 64 lowercase hexadecimal characters of a valid, non-small-order Ed25519 public key.
pub fn validate_device_id(value: &str) -> Result<()> {
    if value.len() != 64
        || value
            .bytes()
            .any(|b| !matches!(b, b'0'..=b'9' | b'a'..=b'f'))
    {
        return Err("invalid device ID".into());
    }
    let key =
        VerifyingKey::from_bytes(&decode32(value)?).map_err(|_| "invalid device public key")?;
    if key.is_weak() {
        return Err("weak device public key".into());
    }
    Ok(())
}

fn decode32(value: &str) -> Result<[u8; 32]> {
    let mut bytes = [0u8; 32];
    hex::decode_to_slice(value, &mut bytes).map_err(|_| "expected a 32-byte hexadecimal key")?;
    Ok(bytes)
}

/// Binds a device key to one session's Noise static key; the device key never encrypts.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Proof {
    device_id: String,
    signature: String,
    payload: Vec<u8>,
}

fn static_message(context: &[u8], static_key: &[u8]) -> Vec<u8> {
    [NOISE_STATIC_DOMAIN, context, static_key].concat()
}

/// Noise XX with a fresh static key per session. The local device signs that key
/// (`signingBytes`, then `setProof`) before its identity can be sent.
#[cfg_attr(target_arch = "wasm32", wasm_bindgen)]
pub struct Handshake {
    inner: Option<snow::HandshakeState>,
    context: Vec<u8>,
    static_public: Vec<u8>,
    proof: Option<(String, String)>,
    expected_device_id: Option<String>,
    remote_id: Option<String>,
    first_write: bool,
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen)]
impl Handshake {
    /// An unpinned responder still requires application membership authorization.
    #[cfg_attr(target_arch = "wasm32", wasm_bindgen(constructor))]
    pub fn new(
        initiator: bool,
        expected_device_id: Option<String>,
        context: &[u8],
    ) -> Result<Handshake> {
        if context.len() > MAX_CONTEXT {
            return Err("Noise context too large".into());
        }
        if let Some(expected) = &expected_device_id {
            validate_device_id(expected)?;
        }
        let builder = snow::Builder::new(NOISE.parse().map_err(|_| "invalid Noise protocol")?);
        let keys = builder
            .generate_keypair()
            .map_err(|_| "Noise key generation failed")?;
        let private = Zeroizing::new(keys.private);
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
            context: context.to_vec(),
            static_public: keys.public,
            proof: None,
            expected_device_id,
            remote_id: None,
            first_write: initiator,
        })
    }

    /// `"vibeface:noise-static:v1" ‖ prologue ‖ this session's static public key`.
    #[cfg_attr(target_arch = "wasm32", wasm_bindgen(js_name = signingBytes))]
    pub fn signing_bytes(&self) -> Vec<u8> {
        static_message(&self.context, &self.static_public)
    }

    /// Accepts the device's signature over `signingBytes` and checks it before use.
    #[cfg_attr(target_arch = "wasm32", wasm_bindgen(js_name = setProof))]
    pub fn set_proof(&mut self, device_id: &str, signature: &str) -> Result<()> {
        validate_device_id(device_id)?;
        verify_message(device_id, &self.signing_bytes(), signature)?;
        self.proof = Some((device_id.to_owned(), signature.to_owned()));
        Ok(())
    }

    /// Application data is accepted only after the remote identity is authenticated.
    pub fn write(&mut self, payload: &[u8]) -> Result<Vec<u8>> {
        let result = self.write_inner(payload);
        if result.is_err() {
            self.inner = None;
            self.remote_id = None;
        }
        result
    }

    pub fn read(&mut self, message: &[u8]) -> Result<Vec<u8>> {
        let result = self.read_inner(message);
        if result.is_err() {
            self.inner = None;
            self.remote_id = None;
        }
        result
    }

    #[cfg_attr(target_arch = "wasm32", wasm_bindgen(js_name = remoteId))]
    pub fn remote_id(&self) -> Option<String> {
        self.remote_id.clone()
    }

    #[cfg_attr(target_arch = "wasm32", wasm_bindgen(js_name = isFinished))]
    pub fn is_finished(&self) -> bool {
        self.remote_id.is_some()
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
    /// A Node signs its own static key with its device key.
    pub fn signed(
        initiator: bool,
        identity: &Identity,
        expected_device_id: Option<String>,
        context: &[u8],
    ) -> Result<Handshake> {
        let mut handshake = Self::new(initiator, expected_device_id, context)?;
        let signature = identity.sign_message(&handshake.signing_bytes());
        handshake.set_proof(&identity.device_id(), &signature)?;
        Ok(handshake)
    }

    fn write_inner(&mut self, payload: &[u8]) -> Result<Vec<u8>> {
        if self.remote_id.is_none() && !payload.is_empty() {
            return Err(
                "Noise identity must be authenticated before sending application data".into(),
            );
        }
        let inner = self.inner.as_mut().ok_or("Noise handshake closed")?;
        let payload = if self.first_write {
            Vec::new()
        } else {
            let (device_id, signature) =
                self.proof.clone().ok_or("device proof missing for Noise")?;
            serde_json::to_vec(&Proof {
                device_id,
                signature,
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
        validate_device_id(&proof.device_id)?;
        verify_message(
            &proof.device_id,
            &static_message(&self.context, remote_static),
            &proof.signature,
        )?;
        if self
            .expected_device_id
            .as_ref()
            .is_some_and(|expected| expected != &proof.device_id)
        {
            return Err("unexpected Noise device identity".into());
        }
        self.remote_id = Some(proof.device_id);
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
        let ah = Handshake::signed(true, &a, Some(b.device_id()), b"vibeface:channel:v1").unwrap();
        let bh = Handshake::signed(false, &b, Some(a.device_id()), b"vibeface:channel:v1").unwrap();
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
        a.expected_device_id = Some(Identity::generate().unwrap().device_id());
        b.read(&a.write(&[]).unwrap()).unwrap();
        assert!(a.read(&b.write(&[]).unwrap()).is_err());
        assert!(a.remote_id().is_none());
        assert!(a.finish().is_err());
        assert!(a.write(&[]).is_err());
    }

    #[test]
    fn substituted_identity_signature_fails_closed() {
        let (_, _, mut a, mut b) = endpoints();
        let device = b.proof.clone().unwrap().0;
        b.proof = Some((device, "00".repeat(64)));
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
        assert_eq!(a.device_id(), restored.device_id());
        let signature = a.sign("client-challenge", b"challenge").unwrap();
        verify(&a.device_id(), "client-challenge", b"challenge", &signature).unwrap();
        assert!(verify(&a.device_id(), "pair", b"challenge", &signature).is_err());
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
        let mut responder = Handshake::signed(false, &remote, None, b"another-service").unwrap();
        responder.read(&initiator.write(&[]).unwrap()).unwrap();
        assert!(initiator.read(&responder.write(&[]).unwrap()).is_err());
    }
    #[test]
    fn an_externally_signed_proof_authenticates_without_the_private_key() {
        let browser = Identity::generate().unwrap();
        let node = Identity::generate().unwrap();
        let mut client =
            Handshake::new(true, Some(node.device_id()), b"vibeface:client:v1").unwrap();
        let other = Identity::generate().unwrap();
        assert!(
            client
                .set_proof(
                    &browser.device_id(),
                    &other.sign_message(&client.signing_bytes())
                )
                .is_err()
        );
        let signature = browser.sign_message(&client.signing_bytes());
        client.set_proof(&browser.device_id(), &signature).unwrap();
        let mut server = Handshake::signed(false, &node, None, b"vibeface:client:v1").unwrap();
        server.read(&client.write(&[]).unwrap()).unwrap();
        client.read(&server.write(&[]).unwrap()).unwrap();
        server.read(&client.write(&[]).unwrap()).unwrap();
        assert_eq!(server.remote_id(), Some(browser.device_id()));
        assert_eq!(client.remote_id(), Some(node.device_id()));
        assert!(client.finish().is_ok() && server.finish().is_ok());
    }

    #[test]
    fn a_proof_for_another_session_key_is_rejected() {
        let browser = Identity::generate().unwrap();
        let node = Identity::generate().unwrap();
        let earlier = Handshake::new(true, None, b"vibeface:client:v1").unwrap();
        let replayed = browser.sign_message(&earlier.signing_bytes());
        let mut client = Handshake::new(true, None, b"vibeface:client:v1").unwrap();
        assert!(client.set_proof(&browser.device_id(), &replayed).is_err());
        client.proof = Some((browser.device_id(), replayed));
        let mut server = Handshake::signed(false, &node, None, b"vibeface:client:v1").unwrap();
        server.read(&client.write(&[]).unwrap()).unwrap();
        client.read(&server.write(&[]).unwrap()).unwrap();
        assert!(server.read(&client.write(&[]).unwrap()).is_err());
        assert!(server.remote_id().is_none());
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
