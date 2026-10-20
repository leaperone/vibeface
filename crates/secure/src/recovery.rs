use argon2::{Algorithm, Argon2, Params, Version};
use chacha20poly1305::{
    XChaCha20Poly1305, XNonce,
    aead::{Aead, KeyInit, Payload},
};
use serde::{Deserialize, Serialize};
#[cfg(target_arch = "wasm32")]
use wasm_bindgen::prelude::*;
use zeroize::Zeroizing;

use crate::Result;

const MEMORY_KIB: u32 = 19_456;
const ITERATIONS: u32 = 2;
const PARALLELISM: u32 = 1;
const MAX_PAYLOAD: usize = 64 * 1024;
const MAX_ENVELOPE: usize = MAX_PAYLOAD * 2 + 4096;

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Header {
    format: String,
    version: u8,
    kdf: String,
    memory_kib: u32,
    iterations: u32,
    parallelism: u32,
    salt: String,
    cipher: String,
    nonce: String,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Envelope {
    header: Header,
    ciphertext: String,
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen(js_name = recoverySeal))]
pub fn recovery_seal(payload: &[u8], password: &str) -> Result<Vec<u8>> {
    validate_password(password)?;
    validate_payload(payload)?;
    let mut salt = [0u8; 16];
    let mut nonce = [0u8; 24];
    getrandom::fill(&mut salt).map_err(|_| "secure randomness unavailable")?;
    getrandom::fill(&mut nonce).map_err(|_| "secure randomness unavailable")?;
    let header = Header {
        format: "vibeface-recovery".into(),
        version: 1,
        kdf: "argon2id".into(),
        memory_kib: MEMORY_KIB,
        iterations: ITERATIONS,
        parallelism: PARALLELISM,
        salt: hex::encode(salt),
        cipher: "xchacha20poly1305".into(),
        nonce: hex::encode(nonce),
    };
    let aad = serde_json::to_vec(&header).map_err(|_| "recovery header encoding failed")?;
    let key = derive_key(password, &salt)?;
    let cipher =
        XChaCha20Poly1305::new_from_slice(key.as_ref()).map_err(|_| "invalid recovery key")?;
    let ciphertext = cipher
        .encrypt(
            XNonce::from_slice(&nonce),
            Payload {
                msg: payload,
                aad: &aad,
            },
        )
        .map_err(|_| "recovery encryption failed")?;
    serde_json::to_vec(&Envelope {
        header,
        ciphertext: hex::encode(ciphertext),
    })
    .map_err(|_| "recovery package encoding failed".into())
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen(js_name = recoveryOpen))]
pub fn recovery_open(envelope: &[u8], password: &str) -> Result<Vec<u8>> {
    validate_password(password)?;
    if envelope.len() > MAX_ENVELOPE {
        return Err("recovery package too large".into());
    }
    let envelope: Envelope =
        serde_json::from_slice(envelope).map_err(|_| "invalid recovery package")?;
    let header = &envelope.header;
    if header.format != "vibeface-recovery"
        || header.version != 1
        || header.kdf != "argon2id"
        || header.memory_kib != MEMORY_KIB
        || header.iterations != ITERATIONS
        || header.parallelism != PARALLELISM
        || header.cipher != "xchacha20poly1305"
    {
        return Err("unsupported recovery format or KDF parameters".into());
    }
    let mut salt = [0u8; 16];
    let mut nonce = [0u8; 24];
    hex::decode_to_slice(&header.salt, &mut salt).map_err(|_| "invalid recovery salt")?;
    hex::decode_to_slice(&header.nonce, &mut nonce).map_err(|_| "invalid recovery nonce")?;
    if envelope.ciphertext.len() < 32 || envelope.ciphertext.len() > (MAX_PAYLOAD + 16) * 2 {
        return Err("invalid recovery ciphertext size".into());
    }
    let ciphertext =
        hex::decode(&envelope.ciphertext).map_err(|_| "invalid recovery ciphertext")?;
    let aad = serde_json::to_vec(header).map_err(|_| "recovery header encoding failed")?;
    let key = derive_key(password, &salt)?;
    let cipher =
        XChaCha20Poly1305::new_from_slice(key.as_ref()).map_err(|_| "invalid recovery key")?;
    let payload = cipher
        .decrypt(
            XNonce::from_slice(&nonce),
            Payload {
                msg: &ciphertext,
                aad: &aad,
            },
        )
        .map_err(|_| "incorrect recovery password or damaged package")?;
    validate_payload(&payload)?;
    Ok(payload)
}

fn derive_key(password: &str, salt: &[u8]) -> Result<Zeroizing<[u8; 32]>> {
    let params = Params::new(MEMORY_KIB, ITERATIONS, PARALLELISM, Some(32))
        .map_err(|_| "invalid recovery KDF configuration")?;
    let mut output = Zeroizing::new([0u8; 32]);
    Argon2::new(Algorithm::Argon2id, Version::V0x13, params)
        .hash_password_into(password.as_bytes(), salt, output.as_mut())
        .map_err(|_| "recovery key derivation failed")?;
    Ok(output)
}

fn validate_password(password: &str) -> Result<()> {
    if !(12..=1024).contains(&password.len()) {
        return Err("recovery password must contain 12 to 1024 UTF-8 bytes".into());
    }
    Ok(())
}

fn validate_payload(payload: &[u8]) -> Result<()> {
    if payload.len() > MAX_PAYLOAD {
        return Err("recovery payload too large".into());
    }
    serde_json::from_slice::<serde_json::Value>(payload)
        .map_err(|_| "recovery payload must be valid JSON")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recovery_roundtrip_wrong_password_and_authenticated_metadata() {
        let payload = br#"{"private":"fixture secret"}"#;
        let password = "correct horse battery staple";
        let encoded = recovery_seal(payload, password).unwrap();
        assert!(!String::from_utf8_lossy(&encoded).contains("fixture secret"));
        assert_eq!(recovery_open(&encoded, password).unwrap(), payload);
        assert!(recovery_open(&encoded, "incorrect fixture password").is_err());
        let mut package: Envelope = serde_json::from_slice(&encoded).unwrap();
        package.header.nonce = "00".repeat(24);
        assert!(recovery_open(&serde_json::to_vec(&package).unwrap(), password).is_err());
        package.header.memory_kib = u32::MAX;
        assert!(
            recovery_open(&serde_json::to_vec(&package).unwrap(), password)
                .unwrap_err()
                .to_string()
                .contains("KDF")
        );
    }

    #[test]
    fn recovery_rejects_unbounded_inputs_before_kdf() {
        assert!(recovery_seal(b"{}", "short").is_err());
        assert!(recovery_seal(&vec![b' '; MAX_PAYLOAD + 1], "correct fixture password").is_err());
        assert!(recovery_open(&vec![b' '; MAX_ENVELOPE + 1], "correct fixture password").is_err());
        assert!(recovery_seal(b"not JSON", "correct fixture password").is_err());
    }
}
