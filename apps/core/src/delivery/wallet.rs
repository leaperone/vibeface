use super::chain::{self, ChainConfig, SignedBytes};
use super::store::SpendIntent;
use alloy_consensus::{SignableTransaction, TxEnvelope};
use alloy_eips::eip2718::Encodable2718;
use alloy_primitives::{Address, B256, Signature};
use anyhow::{Context, Result, ensure};
use argon2::{Algorithm, Argon2, Params, Version};
use chacha20poly1305::{
    ChaCha20Poly1305, KeyInit, Nonce,
    aead::{Aead, Payload},
};
use k256::ecdsa::SigningKey;
use rand::{RngCore, rngs::OsRng};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use vibeface_bridge::platform::privatefs;
use vibeface_control::maintenance::{SecretString, WalletCommand};
use zeroize::Zeroizing;

const KEYSTORE_DOMAIN: &[u8] = b"vibeface-delivery-wallet-v1";
const ARGON_MEMORY_KIB: u32 = 65_536;
const ARGON_PASSES: u32 = 3;
const MAX_KEYSTORE_BYTES: u64 = 4096;

fn validate_passphrase(passphrase: &SecretString) -> Result<()> {
    ensure!(
        (12..=1024).contains(&passphrase.as_str().len()),
        "wallet passphrase must contain 12 to 1024 bytes"
    );
    ensure!(
        !passphrase
            .as_str()
            .bytes()
            .any(|byte| matches!(byte, b'\n' | b'\r' | 0)),
        "wallet passphrase must occupy one line"
    );
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
pub enum WalletStatus {
    Absent,
    Unavailable { reason: String },
    Locked { address: String },
    Unlocked { address: String },
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Keystore {
    format: String,
    address: String,
    salt: String,
    nonce: String,
    ciphertext: String,
}

pub struct Wallet {
    path: PathBuf,
    address: Option<Address>,
    key: Option<SigningKey>,
    load_error: Option<String>,
}

impl Wallet {
    pub fn open(path: PathBuf) -> Result<Self> {
        let loaded = match read_keystore(&path) {
            Ok(store) => Ok(Some(chain::address(&store.address)?)),
            Err(error)
                if error
                    .downcast_ref::<std::io::Error>()
                    .is_some_and(|e| e.kind() == std::io::ErrorKind::NotFound) =>
            {
                Ok(None)
            }
            Err(_) => Err("delivery wallet is unavailable; restore its independent encrypted backup locally and restart the Node".to_owned()),
        };
        let (address, load_error) = match loaded {
            Ok(address) => (address, None),
            Err(reason) => (None, Some(reason)),
        };
        Ok(Self {
            path,
            address,
            key: None,
            load_error,
        })
    }

    pub fn status(&self) -> WalletStatus {
        if let Some(reason) = &self.load_error {
            return WalletStatus::Unavailable {
                reason: reason.clone(),
            };
        }
        match self.address {
            None => WalletStatus::Absent,
            Some(address) if self.key.is_some() => WalletStatus::Unlocked {
                address: chain::address_hex(address),
            },
            Some(address) => WalletStatus::Locked {
                address: chain::address_hex(address),
            },
        }
    }

    pub fn address(&self) -> Result<Address> {
        self.require_loaded()?;
        self.address.context("delivery wallet has not been created")
    }

    pub fn create(&mut self, passphrase: &SecretString) -> Result<WalletStatus> {
        self.require_loaded()?;
        validate_passphrase(passphrase)?;
        ensure!(
            self.address.is_none() && !self.path.try_exists()?,
            "delivery wallet already exists"
        );
        let key = SigningKey::random(&mut OsRng);
        let address = Address::from_public_key(key.verifying_key());
        let mut salt = [0; 32];
        let mut nonce = [0; 12];
        OsRng.fill_bytes(&mut salt);
        OsRng.fill_bytes(&mut nonce);
        let encryption_key = derive_key(passphrase, &salt)?;
        let key_bytes = Zeroizing::new(key.to_bytes());
        let aad = authenticated_metadata(address);
        let ciphertext = ChaCha20Poly1305::new_from_slice(encryption_key.as_ref())
            .map_err(|_| anyhow::anyhow!("invalid wallet encryption key"))?
            .encrypt(
                Nonce::from_slice(&nonce),
                Payload {
                    msg: key_bytes.as_ref(),
                    aad: &aad,
                },
            )
            .map_err(|_| anyhow::anyhow!("wallet encryption failed"))?;
        let store = Keystore {
            format: "vibeface-delivery-wallet-v1".into(),
            address: chain::address_hex(address),
            salt: hex::encode(salt),
            nonce: hex::encode(nonce),
            ciphertext: hex::encode(ciphertext),
        };
        write_new_private(&self.path, &serde_json::to_vec(&store)?)?;
        self.address = Some(address);
        self.key = None;
        Ok(self.status())
    }

    pub fn unlock(&mut self, passphrase: &SecretString) -> Result<WalletStatus> {
        self.require_loaded()?;
        validate_passphrase(passphrase)?;
        let store = read_keystore(&self.path)?;
        let address = chain::address(&store.address)?;
        ensure!(
            self.address == Some(address),
            "delivery wallet changed; restart before unlocking"
        );
        let salt = hex::decode(&store.salt)?;
        let nonce = hex::decode(&store.nonce)?;
        let ciphertext = hex::decode(&store.ciphertext)?;
        let encryption_key = derive_key(passphrase, &salt)?;
        let aad = authenticated_metadata(address);
        let plaintext = Zeroizing::new(
            ChaCha20Poly1305::new_from_slice(encryption_key.as_ref())
                .map_err(|_| anyhow::anyhow!("invalid wallet encryption key"))?
                .decrypt(
                    Nonce::from_slice(&nonce),
                    Payload {
                        msg: &ciphertext,
                        aad: &aad,
                    },
                )
                .map_err(|_| anyhow::anyhow!("wallet passphrase or encrypted backup is invalid"))?,
        );
        let key = SigningKey::from_slice(plaintext.as_ref()).context("wallet secret is invalid")?;
        ensure!(
            Address::from_public_key(key.verifying_key()) == address,
            "wallet address does not match its encrypted key"
        );
        self.key = Some(key);
        Ok(self.status())
    }

    pub fn lock(&mut self) -> WalletStatus {
        self.key = None;
        self.status()
    }

    pub fn sign_quote(
        &self,
        config: &ChainConfig,
        quote_digest: [u8; 32],
        provider_peer: [u8; 32],
    ) -> Result<Vec<u8>> {
        config.validate()?;
        let signature = self.sign_hash(chain::quote_wallet_digest(
            config,
            quote_digest,
            provider_peer,
        )?)?;
        Ok(signature.as_bytes().to_vec())
    }

    pub fn sign_transaction(
        &self,
        config: &ChainConfig,
        intent: &SpendIntent,
        nonce: u64,
    ) -> Result<SignedBytes> {
        let wallet = self.address()?;
        let transaction = chain::checked_transaction(config, intent, wallet, nonce)?;
        self.sign_checked_transaction(transaction)
    }

    pub(super) fn sign_publication_recovery(
        &self,
        config: &ChainConfig,
        original: &super::store::SignedTransaction,
        intent: &SpendIntent,
    ) -> Result<SignedBytes> {
        let transaction =
            chain::checked_publication_recovery(config, original, intent, self.address()?)?;
        self.sign_checked_transaction(transaction)
    }

    fn sign_checked_transaction(
        &self,
        transaction: alloy_consensus::TxEip1559,
    ) -> Result<SignedBytes> {
        let signature = self.sign_hash(transaction.signature_hash())?;
        let envelope = TxEnvelope::from(transaction.into_signed(signature));
        let raw = envelope.encoded_2718();
        Ok(SignedBytes {
            raw_tx: format!("0x{}", hex::encode(&raw)),
            tx_hash: format!("0x{}", hex::encode(alloy_primitives::keccak256(raw))),
        })
    }

    fn sign_hash(&self, digest: B256) -> Result<Signature> {
        self.require_loaded()?;
        let key = self.key.as_ref().context(
            "delivery wallet is locked; unlock it through the local maintenance command",
        )?;
        let (signature, recovery) = key
            .sign_prehash_recoverable(digest.as_slice())
            .context("delivery wallet signature failed")?;
        ensure!(
            !recovery.is_x_reduced(),
            "unsupported delivery wallet signature recovery id"
        );
        Ok(Signature::from_bytes_and_parity(
            signature.to_bytes().as_slice(),
            recovery.is_y_odd(),
        ))
    }

    fn require_loaded(&self) -> Result<()> {
        if let Some(reason) = &self.load_error {
            anyhow::bail!("{reason}");
        }
        Ok(())
    }
}

fn derive_key(passphrase: &SecretString, salt: &[u8]) -> Result<Zeroizing<[u8; 32]>> {
    ensure!(salt.len() == 32, "invalid wallet salt length");
    let params = Params::new(ARGON_MEMORY_KIB, ARGON_PASSES, 1, Some(32))
        .map_err(|_| anyhow::anyhow!("invalid wallet password parameters"))?;
    let mut key = Zeroizing::new([0; 32]);
    Argon2::new(Algorithm::Argon2id, Version::V0x13, params)
        .hash_password_into(passphrase.as_str().as_bytes(), salt, key.as_mut())
        .map_err(|_| anyhow::anyhow!("wallet password derivation failed"))?;
    Ok(key)
}

fn authenticated_metadata(address: Address) -> Vec<u8> {
    [KEYSTORE_DOMAIN, address.as_slice()].concat()
}

fn read_keystore(path: &Path) -> Result<Keystore> {
    let bytes = privatefs::read_private_bounded(path, MAX_KEYSTORE_BYTES, "the delivery wallet")?;
    let store: Keystore = serde_json::from_slice(&bytes)?;
    ensure!(
        store.format == "vibeface-delivery-wallet-v1"
            && store.salt.len() == 64
            && store.nonce.len() == 24
            && store.ciphertext.len() == 96,
        "invalid encrypted delivery wallet format"
    );
    chain::address(&store.address)?;
    for field in [&store.salt, &store.nonce, &store.ciphertext] {
        ensure!(
            field
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase()),
            "invalid encrypted delivery wallet encoding"
        );
    }
    Ok(store)
}

fn write_new_private(path: &Path, bytes: &[u8]) -> Result<()> {
    let result = privatefs::create_durable(path, bytes);
    // A file that was already there is not this call's to remove.
    if let Err(error) = &result
        && !error
            .downcast_ref::<std::io::Error>()
            .is_some_and(|e| e.kind() == std::io::ErrorKind::AlreadyExists)
    {
        let _ = std::fs::remove_file(path);
    }
    result
}

pub fn backup(data_dir: &Path, output: &Path) -> Result<()> {
    let source = data_dir.join("delivery-wallet.json");
    let store = read_keystore(&source)?;
    write_new_private(output, &serde_json::to_vec(&store)?)
}

impl super::Delivery {
    pub async fn local_wallet(&self, command: WalletCommand) -> Result<WalletStatus> {
        if matches!(
            command,
            WalletCommand::Create { .. } | WalletCommand::Unlock { .. }
        ) {
            ensure!(
                self.config().enabled,
                "enable the experimental delivery service before creating or unlocking its wallet"
            );
        }
        let mut wallet = self
            .wallet
            .lock()
            .map_err(|_| anyhow::anyhow!("delivery wallet lock failed"))?;
        match command {
            WalletCommand::Status => Ok(wallet.status()),
            WalletCommand::Create { passphrase } => wallet.create(&passphrase),
            WalletCommand::Unlock { passphrase } => wallet.unlock(&passphrase),
            WalletCommand::Lock => Ok(wallet.lock()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encrypted_wallet_restarts_locked_and_rejects_wrong_password_without_changing_bytes()
    -> Result<()> {
        let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../.artifacts/wallet-tests")
            .join(uuid::Uuid::new_v4().to_string());
        crate::config::private_dir(&dir)?;
        let path = dir.join("delivery-wallet.json");
        let password = SecretString::new("synthetic test wallet passphrase".into());
        let mut wallet = Wallet::open(path.clone())?;
        assert_eq!(wallet.status(), WalletStatus::Absent);
        wallet.create(&password)?;
        assert!(matches!(wallet.status(), WalletStatus::Locked { .. }));
        let encrypted = std::fs::read(&path)?;
        assert!(
            wallet
                .unlock(&SecretString::new("wrong synthetic passphrase".into()))
                .is_err()
        );
        assert_eq!(std::fs::read(&path)?, encrypted);
        wallet.unlock(&password)?;
        assert!(matches!(wallet.status(), WalletStatus::Unlocked { .. }));
        assert!(matches!(
            Wallet::open(path.clone())?.status(),
            WalletStatus::Locked { .. }
        ));
        assert!(wallet.create(&password).is_err());
        wallet.lock();
        let config = chain::tests::config();
        assert!(wallet.sign_quote(&config, [7; 32], [8; 32]).is_err());
        let output = dir.join("wallet-backup.json");
        backup(&dir, &output)?;
        assert_eq!(std::fs::read(&output)?, encrypted);
        assert!(backup(&dir, &output).is_err());
        assert_eq!(std::fs::read(&output)?, encrypted);
        std::fs::remove_dir_all(dir)?;
        Ok(())
    }

    #[test]
    fn wallet_quote_uses_contract_eip712_domain_and_exact_wallet() -> Result<()> {
        let key = SigningKey::from_slice(&[1; 32])?;
        let wallet = Wallet {
            path: PathBuf::new(),
            address: Some(Address::from_public_key(key.verifying_key())),
            key: Some(key),
            load_error: None,
        };
        let config = chain::tests::config();
        let digest = chain::quote_wallet_digest(&config, [2; 32], [3; 32])?;
        let signature = Signature::from_raw(&wallet.sign_quote(&config, [2; 32], [3; 32])?)?;
        assert_eq!(
            signature.recover_address_from_prehash(&digest)?,
            wallet.address()?
        );
        let mut wrong = config.clone();
        wrong.escrow = "0x9999999999999999999999999999999999999999".into();
        assert_ne!(
            signature.recover_address_from_prehash(&chain::quote_wallet_digest(
                &wrong, [2; 32], [3; 32]
            )?)?,
            wallet.address()?
        );
        Ok(())
    }

    #[test]
    fn transaction_signature_preserves_the_reserved_nonce_and_retries_exact_bytes() -> Result<()> {
        use alloy_consensus::{Transaction, transaction::SignerRecoverable};
        use alloy_eips::eip2718::Decodable2718;
        let key = SigningKey::from_slice(&[2; 32])?;
        let mut wallet = Wallet {
            path: PathBuf::new(),
            address: Some(Address::from_public_key(key.verifying_key())),
            key: Some(key),
            load_error: None,
        };
        let intent = chain::tests::approval_intent(wallet.address()?)?;
        let config = chain::tests::config();
        let first = wallet.sign_transaction(&config, &intent, 7)?;
        assert_eq!(wallet.sign_transaction(&config, &intent, 7)?, first);
        assert_ne!(
            wallet.sign_transaction(&config, &intent, 8)?.tx_hash,
            first.tx_hash
        );
        let raw = hex::decode(first.raw_tx.trim_start_matches("0x"))?;
        let envelope = TxEnvelope::decode_2718(&mut raw.as_slice())?;
        assert_eq!(envelope.recover_signer()?, wallet.address()?);
        assert_eq!(envelope.nonce(), 7);
        assert_eq!(envelope.chain_id(), Some(config.chain_id));
        assert_eq!(
            envelope.kind(),
            alloy_primitives::TxKind::Call(chain::address(&config.asset)?)
        );
        let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../.artifacts/wallet-tests")
            .join(uuid::Uuid::new_v4().to_string());
        crate::config::private_dir(&dir)?;
        let mut store = crate::delivery::store::ServiceStore::open(&dir, 1_000_000, 8)?;
        let mut original = intent.clone();
        original.id = "provider/approve".into();
        let signed = store.signed_budget_transaction(
            &original,
            7,
            "provider",
            &original.gas_cap,
            |intent, nonce| {
                let signed = wallet.sign_transaction(&config, intent, nonce)?;
                Ok((signed.raw_tx, signed.tx_hash))
            },
        )?;
        assert_eq!(signed.raw_tx.as_deref(), Some(first.raw_tx.as_str()));
        assert_eq!(signed.tx_hash.as_deref(), Some(first.tx_hash.as_str()));
        wallet.lock();
        let mut next = original.clone();
        next.id = "key-original/register".into();
        assert!(
            store
                .signed_budget_transaction(
                    &next,
                    8,
                    "key-original",
                    &next.gas_cap,
                    |intent, nonce| {
                        let signed = wallet.sign_transaction(&config, intent, nonce)?;
                        Ok((signed.raw_tx, signed.tx_hash))
                    }
                )
                .is_err()
        );
        assert!(store.find_transaction(&next.id)?.is_none());
        assert_eq!(store.transaction(&original.id)?, signed);
        drop(store);
        std::fs::remove_dir_all(dir)?;
        Ok(())
    }

    #[test]
    fn nonce_recovery_signer_only_spends_the_original_reveal_nonce_on_the_pinned_getter()
    -> Result<()> {
        use alloy_consensus::Transaction;
        use alloy_eips::eip2718::Decodable2718;
        use alloy_sol_types::SolCall;
        let key = SigningKey::from_slice(&[2; 32])?;
        let wallet = Wallet {
            path: PathBuf::new(),
            address: Some(Address::from_public_key(key.verifying_key())),
            key: Some(key),
            load_error: None,
        };
        let config = chain::tests::config();
        let mut intent = chain::tests::approval_intent(wallet.address()?)?;
        let (target, calldata) = super::super::budget::BudgetAction::Reveal {
            budget_id: [3; 32],
            items: vec![super::super::budget::Reveal {
                window: super::super::budget::WindowRef {
                    order_id: "04".repeat(32),
                    index: 0,
                },
                capsule: vec![5; 80],
            }],
        }
        .encode(&config, wallet.address()?)?;
        intent.id = format!("budget-{}/reveal-{}-0", "03".repeat(32), "06".repeat(8));
        intent.to = chain::address_hex(target);
        intent.calldata = format!("0x{}", hex::encode(calldata));
        intent.gas_limit = super::super::budget::REVEAL_GAS_LIMIT;
        intent.gas_cap = "100000000000000000".into();
        let signed = wallet.sign_transaction(&config, &intent, 7)?;
        let original = super::super::store::SignedTransaction {
            intent,
            nonce: 7,
            raw_tx: Some(signed.raw_tx),
            tx_hash: Some(signed.tx_hash),
            phase: "signed".into(),
            block_number: None,
            block_hash: None,
            error: None,
        };
        let mut intent = original.intent.clone();
        intent.id = format!("{}/nonce-0", original.intent.id);
        intent.gas_limit = chain::NONCE_RECOVERY_GAS_LIMIT;
        intent.calldata = format!(
            "0x{}",
            hex::encode(chain::abi::profileIdCall {}.abi_encode())
        );
        let signed = wallet.sign_publication_recovery(&config, &original, &intent)?;
        assert!(
            wallet.sign_transaction(&config, &intent, 7).is_err(),
            "nonce recovery must not widen the ordinary action whitelist"
        );
        let raw = hex::decode(signed.raw_tx.trim_start_matches("0x"))?;
        let transaction = TxEnvelope::decode_2718(&mut raw.as_slice())?;
        assert_eq!(transaction.nonce(), 7);
        assert_eq!(transaction.kind(), alloy_primitives::TxKind::Call(target));
        assert_eq!(transaction.value(), alloy_primitives::U256::ZERO);
        assert_eq!(
            transaction.input().as_ref(),
            chain::abi::profileIdCall {}.abi_encode()
        );
        for case in 0..8 {
            let mut wrong = intent.clone();
            match case {
                0 => wrong.chain_id += 1,
                1 => wrong.wallet = chain::address_hex(Address::repeat_byte(9)),
                2 => wrong.to = wrong.wallet.clone(),
                3 => wrong.value = "1".into(),
                4 => wrong.calldata.push_str("00"),
                5 => wrong.gas_limit += 1,
                6 => wrong.gas_cap = "200000000000000000".into(),
                7 => wrong.max_fee_per_gas = "1000000000000".into(),
                _ => unreachable!(),
            }
            assert!(
                wallet
                    .sign_publication_recovery(&config, &original, &wrong)
                    .is_err()
            );
        }
        let mut wrong_original = original;
        wrong_original.intent = chain::tests::approval_intent(wallet.address()?)?;
        assert!(
            wallet
                .sign_publication_recovery(&config, &wrong_original, &intent)
                .is_err()
        );
        Ok(())
    }

    #[test]
    fn corrupt_optional_wallet_stays_unavailable_without_overwriting_its_backup() -> Result<()> {
        let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../.artifacts/wallet-tests")
            .join(uuid::Uuid::new_v4().to_string());
        crate::config::private_dir(&dir)?;
        let path = dir.join("delivery-wallet.json");
        write_new_private(&path, b"damaged encrypted fixture")?;
        let original = std::fs::read(&path)?;
        let mut wallet = Wallet::open(path.clone())?;
        assert!(matches!(wallet.status(), WalletStatus::Unavailable { .. }));
        let password = SecretString::new("synthetic test wallet passphrase".into());
        assert!(wallet.create(&password).is_err());
        assert!(wallet.unlock(&password).is_err());
        assert!(
            wallet
                .sign_quote(&chain::tests::config(), [7; 32], [8; 32])
                .is_err()
        );
        assert_eq!(std::fs::read(&path)?, original);
        std::fs::remove_dir_all(dir)?;
        Ok(())
    }
}
