//! Business validation and wire wrapping for password-protected recovery.
//!
//! Password KDF and authenticated encryption are supplied by `vibeface-secure`.
//! This module keeps personal-network metadata and validates the decrypted values.

use crate::{
    crypto::LocalIdentity,
    identity::{Genesis, canonical_json, validate_id},
};
use anyhow::{Result, anyhow, ensure};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{collections::BTreeSet, fmt};
use zeroize::{Zeroize, Zeroizing};

const PACKAGE_VERSION: u32 = 1;
const PASSPHRASE_MIN_BYTES: usize = 12;
const PASSPHRASE_MAX_BYTES: usize = 1024;
const MAX_RECOVERY_KEYS: usize = 64;
const MAX_PAYLOAD_BYTES: usize = 64 * 1024;
const MAX_PACKAGE_BYTES: usize = 256 * 1024;
const KEY_BYTES: usize = 32;

/// A historical content-encryption key retained for recovery.
#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RecoveryKey {
    pub epoch: u64,
    pub key_hex: String,
}

impl fmt::Debug for RecoveryKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RecoveryKey")
            .field("epoch", &self.epoch)
            .field("key_hex", &"[redacted]")
            .finish()
    }
}

/// The cleartext values restored by a recovery package.
#[derive(Clone)]
pub struct RecoverySecrets {
    pub network_id: String,
    pub root: LocalIdentity,
    pub genesis: Genesis,
    pub recovery_epoch: u64,
    pub decryption_keys: Vec<RecoveryKey>,
}

impl PartialEq for RecoverySecrets {
    fn eq(&self, other: &Self) -> bool {
        self.network_id == other.network_id
            && self.root.secret_key_hex == other.root.secret_key_hex
            && self.genesis == other.genesis
            && self.recovery_epoch == other.recovery_epoch
            && self.decryption_keys == other.decryption_keys
    }
}

impl Eq for RecoverySecrets {}

impl fmt::Debug for RecoverySecrets {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RecoverySecrets")
            .field("network_id", &self.network_id)
            .field("root", &self.root)
            .field("genesis", &self.genesis)
            .field("recovery_epoch", &self.recovery_epoch)
            .field("decryption_key_count", &self.decryption_keys.len())
            .finish()
    }
}

/// JSON-compatible opaque wrapper around a `vibeface-secure` envelope.
///
/// The envelope contents are intentionally not interpreted here.  They are
/// authenticated and decrypted only by the native/WASM secure implementation.
#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct EncryptedRecoveryPackage {
    pub version: u32,
    pub network_id: String,
    pub recovery_epoch: u64,
    pub envelope: Value,
}

impl fmt::Debug for EncryptedRecoveryPackage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("EncryptedRecoveryPackage")
            .field("version", &self.version)
            .field("network_id", &self.network_id)
            .field("recovery_epoch", &self.recovery_epoch)
            .field("envelope", &"[redacted]")
            .finish()
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RecoveryPayload {
    version: u32,
    network_id: String,
    root_secret_key: String,
    genesis: Genesis,
    recovery_epoch: u64,
    decryption_keys: Vec<RecoveryKey>,
}

/// Encrypt recovery material with a passphrase.
pub fn seal(secrets: &RecoverySecrets, passphrase: &str) -> Result<EncryptedRecoveryPackage> {
    validate_passphrase(passphrase)?;
    validate_secrets(secrets)?;
    let mut payload = RecoveryPayload {
        version: PACKAGE_VERSION,
        network_id: secrets.network_id.clone(),
        root_secret_key: secrets.root.secret_key_hex.clone(),
        genesis: secrets.genesis.clone(),
        recovery_epoch: secrets.recovery_epoch,
        decryption_keys: secrets.decryption_keys.clone(),
    };
    let payload_bytes = Zeroizing::new(canonical_json(&payload)?);
    payload.root_secret_key.zeroize();
    for key in &mut payload.decryption_keys {
        key.key_hex.zeroize();
    }
    ensure!(
        payload_bytes.len() <= MAX_PAYLOAD_BYTES,
        "recovery payload too large"
    );
    let envelope_bytes = vibeface_secure::recovery_seal(payload_bytes.as_slice(), passphrase)
        .map_err(|_| anyhow!("failed to seal recovery package"))?;
    ensure!(
        envelope_bytes.len() <= MAX_PACKAGE_BYTES,
        "recovery envelope too large"
    );
    let envelope: Value = serde_json::from_slice(&envelope_bytes)
        .map_err(|_| anyhow!("invalid recovery envelope"))?;
    let package = EncryptedRecoveryPackage {
        version: PACKAGE_VERSION,
        network_id: secrets.network_id.clone(),
        recovery_epoch: secrets.recovery_epoch,
        envelope,
    };
    validate_package(&package)?;
    Ok(package)
}

/// Decrypt and validate a recovery package.
pub fn open(package: &EncryptedRecoveryPackage, passphrase: &str) -> Result<RecoverySecrets> {
    validate_passphrase(passphrase)?;
    validate_package(package)?;
    let envelope_bytes =
        serde_json::to_vec(&package.envelope).map_err(|_| anyhow!("invalid recovery envelope"))?;
    let mut payload_bytes = Zeroizing::new(
        vibeface_secure::recovery_open(&envelope_bytes, passphrase)
            .map_err(|_| anyhow!("failed to open recovery package"))?,
    );
    ensure!(
        payload_bytes.len() <= MAX_PAYLOAD_BYTES,
        "recovery payload too large"
    );
    let payload: RecoveryPayload = serde_json::from_slice(payload_bytes.as_slice())
        .map_err(|_| anyhow!("invalid recovery payload"))?;
    payload_bytes.zeroize();
    validate_payload(&payload, package)?;
    let root = LocalIdentity {
        secret_key_hex: payload.root_secret_key,
    };
    let secrets = RecoverySecrets {
        network_id: payload.network_id,
        root,
        genesis: payload.genesis,
        recovery_epoch: payload.recovery_epoch,
        decryption_keys: payload.decryption_keys,
    };
    validate_secrets(&secrets)?;
    Ok(secrets)
}

fn validate_passphrase(passphrase: &str) -> Result<()> {
    ensure!(
        (PASSPHRASE_MIN_BYTES..=PASSPHRASE_MAX_BYTES).contains(&passphrase.len()),
        "recovery passphrase length is invalid"
    );
    Ok(())
}

fn validate_secrets(secrets: &RecoverySecrets) -> Result<()> {
    validate_id(&secrets.network_id)?;
    let root_network = secrets
        .root
        .network_id()
        .map_err(|_| anyhow!("invalid recovery root identity"))?;
    ensure!(
        secrets.network_id == root_network,
        "recovery network does not match root identity"
    );
    secrets.genesis.verify()?;
    ensure!(
        secrets.genesis.network_id == secrets.network_id
            && secrets.genesis.owner_public_key == secrets.root.public_key_hex()?,
        "recovery genesis does not match root identity"
    );
    ensure!(
        (1..=super::MAX_SAFE_INTEGER).contains(&secrets.recovery_epoch),
        "invalid recovery epoch"
    );
    ensure!(
        secrets.decryption_keys.len() <= MAX_RECOVERY_KEYS,
        "too many recovery keys"
    );
    let mut epochs = BTreeSet::new();
    for key in &secrets.decryption_keys {
        ensure!(
            (1..=super::MAX_SAFE_INTEGER).contains(&key.epoch),
            "invalid recovery key epoch"
        );
        ensure!(epochs.insert(key.epoch), "duplicate recovery key epoch");
        validate_key_hex(&key.key_hex)?;
    }
    Ok(())
}

fn validate_payload(payload: &RecoveryPayload, package: &EncryptedRecoveryPackage) -> Result<()> {
    ensure!(
        payload.version == PACKAGE_VERSION,
        "unsupported recovery payload version"
    );
    ensure!(
        payload.network_id == package.network_id,
        "recovery network metadata mismatch"
    );
    ensure!(
        payload.recovery_epoch == package.recovery_epoch,
        "recovery epoch metadata mismatch"
    );
    Ok(())
}

fn validate_package(package: &EncryptedRecoveryPackage) -> Result<()> {
    ensure!(
        package.version == PACKAGE_VERSION,
        "unsupported recovery package version"
    );
    validate_id(&package.network_id)?;
    ensure!(
        (1..=super::MAX_SAFE_INTEGER).contains(&package.recovery_epoch),
        "invalid recovery epoch"
    );
    let envelope_bytes =
        serde_json::to_vec(&package.envelope).map_err(|_| anyhow!("invalid recovery envelope"))?;
    ensure!(!envelope_bytes.is_empty(), "empty recovery envelope");
    ensure!(
        envelope_bytes.len() <= MAX_PACKAGE_BYTES,
        "recovery package too large"
    );
    Ok(())
}

fn validate_key_hex(key_hex: &str) -> Result<()> {
    ensure!(
        key_hex.len() == KEY_BYTES * 2,
        "invalid recovery key length"
    );
    let bytes = hex::decode(key_hex).map_err(|_| anyhow!("invalid recovery key"))?;
    ensure!(bytes.len() == KEY_BYTES, "invalid recovery key");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::identity::{Genesis, Member};

    fn secrets() -> RecoverySecrets {
        let root = LocalIdentity {
            secret_key_hex: "11".repeat(KEY_BYTES),
        };
        let admin = LocalIdentity::generate();
        let member = Member::client(&admin, "admin", true).unwrap();
        let genesis = Genesis::create(&root, vec![member]).unwrap();
        RecoverySecrets {
            network_id: root.network_id().unwrap(),
            root,
            genesis,
            recovery_epoch: 7,
            decryption_keys: vec![RecoveryKey {
                epoch: 3,
                key_hex: "22".repeat(KEY_BYTES),
            }],
        }
    }

    #[test]
    fn roundtrip() {
        let original = secrets();
        let package = seal(&original, "correct horse battery staple").unwrap();
        let restored = open(&package, "correct horse battery staple").unwrap();
        assert_eq!(restored, original);
    }

    #[test]
    fn wrong_passphrase_does_not_reveal_it() {
        let package = seal(&secrets(), "correct horse battery staple").unwrap();
        let error = open(&package, "wrong passphrase that is long enough").unwrap_err();
        let text = format!("{error:#}");
        assert!(!text.contains("wrong passphrase"));
    }

    #[test]
    fn tampered_metadata_and_envelope_are_rejected() {
        let package = seal(&secrets(), "correct horse battery staple").unwrap();
        let mut metadata = package.clone();
        metadata.recovery_epoch += 1;
        assert!(open(&metadata, "correct horse battery staple").is_err());

        let mut envelope = package;
        envelope.envelope = Value::String("tampered".into());
        assert!(open(&envelope, "correct horse battery staple").is_err());
    }

    #[test]
    fn malicious_kdf_and_envelope_size_are_rejected_before_recovery() {
        let mut package = seal(&secrets(), "correct horse battery staple").unwrap();
        let header = package
            .envelope
            .as_object_mut()
            .and_then(|envelope| envelope.get_mut("header"))
            .and_then(Value::as_object_mut)
            .unwrap();
        header.insert("memory_kib".into(), Value::from(u32::MAX));
        assert!(open(&package, "correct horse battery staple").is_err());

        let mut oversized = seal(&secrets(), "correct horse battery staple").unwrap();
        oversized.envelope = Value::String("x".repeat(MAX_PACKAGE_BYTES + 1));
        assert!(open(&oversized, "correct horse battery staple").is_err());
    }

    #[test]
    fn invalid_key_material_is_rejected() {
        let mut invalid = secrets();
        invalid.decryption_keys[0].key_hex = "00".repeat(KEY_BYTES - 1);
        assert!(seal(&invalid, "correct horse battery staple").is_err());
        invalid = secrets();
        invalid.root.secret_key_hex = "zz".repeat(KEY_BYTES);
        assert!(seal(&invalid, "correct horse battery staple").is_err());
    }

    #[test]
    fn debug_never_contains_private_material() {
        let original = secrets();
        let package = seal(&original, "correct horse battery staple").unwrap();
        let secrets_debug = format!("{original:?}");
        let package_debug = format!("{package:?}");
        assert!(!secrets_debug.contains(&original.root.secret_key_hex));
        assert!(!secrets_debug.contains(&original.decryption_keys[0].key_hex));
        assert!(!package_debug.contains(&original.root.secret_key_hex));
        assert!(!package_debug.contains(&original.decryption_keys[0].key_hex));
    }
}
