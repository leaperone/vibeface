//! One-time installation capability for a fresh Node database.

use crate::{crypto, store::Store};
use anyhow::{Context, Result, ensure};
use rand::{RngCore, rngs::OsRng};
use rusqlite::{OptionalExtension, TransactionBehavior};
use serde::{Deserialize, Serialize};

#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct BootstrapCapability {
    pub capability: String,
}
impl BootstrapCapability {
    pub fn generate() -> Self {
        let mut bytes = [0; 32];
        OsRng.fill_bytes(&mut bytes);
        Self {
            capability: hex::encode(bytes),
        }
    }
}
impl std::fmt::Debug for BootstrapCapability {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BootstrapCapability")
            .field("capability", &"[redacted]")
            .finish()
    }
}

fn validate_capability(capability: &str) -> Result<()> {
    ensure!(
        capability.len() == 64
            && capability
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
        "invalid bootstrap capability"
    );
    Ok(())
}

impl Store {
    pub fn install_bootstrap_capability(&mut self, capability: &BootstrapCapability) -> Result<()> {
        validate_capability(&capability.capability)?;
        ensure!(self.policy.pins().is_empty(), "Node is already initialized");
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let initialized: bool =
            tx.query_row("SELECT EXISTS(SELECT 1 FROM policy_pins)", [], |row| {
                row.get(0)
            })?;
        ensure!(!initialized, "Node is already initialized");
        let hash = crypto::digest(capability.capability.as_bytes());
        let previous: Option<String> = tx
            .query_row(
                "SELECT value FROM identity_meta WHERE key='bootstrap_capability_hash'",
                [],
                |row| row.get(0),
            )
            .optional()?;
        if let Some(previous) = previous {
            ensure!(previous == hash, "bootstrap capability already issued");
        } else {
            tx.execute(
                "INSERT INTO identity_meta(key,value) VALUES('bootstrap_capability_hash',?)",
                [&hash],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    pub fn verify_bootstrap_capability(&self, capability: &str) -> Result<()> {
        validate_capability(capability)?;
        ensure!(self.policy.pins().is_empty(), "Node is already initialized");
        let expected: String = self
            .connection
            .query_row(
                "SELECT value FROM identity_meta WHERE key='bootstrap_capability_hash'",
                [],
                |row| row.get(0),
            )
            .context("bootstrap capability is unavailable")?;
        ensure!(
            crypto::digest(capability.as_bytes()) == expected,
            "invalid bootstrap capability"
        );
        Ok(())
    }
}
