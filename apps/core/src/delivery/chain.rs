use super::store::{SignedTransaction, SpendIntent};
use alloy_consensus::TxEip1559;
use alloy_primitives::{Address, B256, TxKind, U256, keccak256};
use alloy_sol_types::{SolCall, SolStruct, SolValue, eip712_domain};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    sync::atomic::{AtomicU64, Ordering},
    time::Duration,
};
use vibeface_delivery::{Amount, ProofProfile, Relation, Statement, Word};

const LOCAL_CHAIN_ID: u64 = 31_337;
const MAX_RPC_BYTES: usize = 512 * 1024;
const MAX_CALLDATA_BYTES: usize = 64 * 1024;
const MAX_GAS_LIMIT: u64 = 30_000_000;
const CONTROL_ROOT: &str = "0xa54dc85ac99f851c92d7c96d7318af41dbe7c0194edfcc37eb4d422a998c1f56";
const BN254_CONTROL_ID: &str = "0x04446e66d300eb7fb45c9726bb53c793dda407a62e9601618bb43c5c14657ac0";

pub(super) mod abi {
    include!(concat!(env!("OUT_DIR"), "/delivery_abi.rs"));
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChainConfig {
    pub rpc_url: String,
    pub chain_id: u64,
    pub escrow: String,
    pub asset: String,
    pub profile: ProofProfile,
    pub confirmations: u64,
}
impl ChainConfig {
    pub fn validate(&self) -> Result<()> {
        let url = reqwest::Url::parse(&self.rpc_url)?;
        let host = url
            .host_str()
            .context("delivery RPC has no host")?
            .trim_matches(['[', ']']);
        let ip: std::net::IpAddr = host
            .parse()
            .context("delivery RPC requires a literal loopback address")?;
        ensure!(
            url.scheme() == "http"
                && ip.is_loopback()
                && url.username().is_empty()
                && url.password().is_none()
                && url.path() == "/"
                && url.query().is_none()
                && url.fragment().is_none(),
            "experimental delivery RPC must be a loopback HTTP origin"
        );
        ensure!(
            self.chain_id == LOCAL_CHAIN_ID,
            "only the local experimental delivery chain is supported"
        );
        ensure!(
            (1..=64).contains(&self.confirmations),
            "invalid delivery confirmation depth"
        );
        let escrow = address(&self.escrow)?;
        let asset = address(&self.asset)?;
        ensure!(
            escrow != Address::ZERO && asset != Address::ZERO && escrow != asset,
            "invalid delivery deployment addresses"
        );
        self.profile.validate()?;
        let expected: ProofProfile = serde_json::from_str(include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../crates/delivery/proof/profile.json"
        )))?;
        ensure!(
            self.profile == expected,
            "delivery profile differs from this build's pinned guest and tested limits"
        );
        Ok(())
    }
}

pub enum Call {
    Budget(Box<super::budget::BudgetAction>),
    Approve {
        amount: Amount,
    },
    RegisterKey {
        statement: Statement,
        seal: Vec<u8>,
    },
    Open {
        order_id: Word,
        index: u16,
        seal: Vec<u8>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SignedBytes {
    pub raw_tx: String,
    pub tx_hash: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct KeyRecord {
    pub binding_hash: Word,
    pub serial: u64,
    pub valid_from: u64,
    pub valid_until: u64,
    pub active: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WindowState {
    pub phase: u8,
    pub decryption_proven: bool,
    pub capsule: Vec<u8>,
    pub ciphertext_hash: Word,
    pub capsule_hash: Word,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OrderState {
    pub payer: String,
    pub provider_wallet: String,
    pub earned: String,
    pub withdrawn: String,
    pub refunded: String,
    pub bonus_earned: bool,
    pub bonus_refunded: bool,
    pub windows: Vec<WindowState>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TxObservation {
    pub phase: String,
    pub tx_hash: Option<String>,
    pub block_number: Option<u64>,
    pub block_hash: Option<String>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChainPosition {
    pub block_number: u64,
    pub block_hash: String,
    pub timestamp: u64,
}

pub struct Chain {
    config: ChainConfig,
    client: reqwest::Client,
    request_id: AtomicU64,
}

impl Chain {
    pub async fn connect(config: ChainConfig) -> Result<Self> {
        config.validate()?;
        let chain = Self {
            config,
            client: reqwest::Client::builder()
                .no_proxy()
                .redirect(reqwest::redirect::Policy::none())
                .connect_timeout(Duration::from_secs(3))
                .timeout(Duration::from_secs(10))
                .build()?,
            request_id: AtomicU64::new(1),
        };
        chain.verify_deployment().await?;
        Ok(chain)
    }

    pub fn config(&self) -> &ChainConfig {
        &self.config
    }

    async fn verify_deployment(&self) -> Result<()> {
        let id = quantity_u64(&self.rpc("eth_chainId", json!([])).await?)?;
        ensure!(
            id == self.config.chain_id,
            "delivery RPC returned a different chain"
        );
        let trust: TrustFile = serde_json::from_str(include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../contracts/delivery/trust.json"
        )))?;
        ensure!(
            trust.schema_version == 1
                && trust.compiler == "0.8.30"
                && trust.statement_schema_hash.trim_start_matches("0x")
                    == hex::encode(vibeface_delivery::schema_hash()),
            "delivery trust artifact does not match this protocol build"
        );
        let escrow = address(&self.config.escrow)?;
        let asset = address(&self.config.asset)?;
        self.verify_code(&trust, "DeliveryEscrow", escrow).await?;
        self.verify_code(&trust, "TestToken", asset).await?;
        let adapter = self.call(escrow, abi::adapterCall {}).await?;
        ensure!(
            adapter != Address::ZERO && adapter != escrow && adapter != asset,
            "invalid delivery verifier adapter"
        );
        self.verify_code(&trust, "RiscZeroAdapter", adapter).await?;
        let verifier = self.call(adapter, abi::verifierCall {}).await?;
        ensure!(
            verifier != Address::ZERO
                && verifier != adapter
                && verifier != escrow
                && verifier != asset,
            "invalid delivery proof verifier"
        );
        self.verify_code(&trust, "RiscZeroGroth16Verifier", verifier)
            .await?;
        let actual_profile = self.call(escrow, abi::getProfileCall {}).await?;
        ensure!(
            actual_profile.abi_encode() == self.config.profile.abi_bytes()?,
            "delivery escrow uses another proof profile"
        );
        ensure!(
            self.call(escrow, abi::profileIdCall {}).await?.as_slice()
                == self.config.profile.digest()?,
            "delivery profile digest mismatch"
        );
        ensure!(
            self.call(adapter, abi::imageIdCall {}).await?.as_slice()
                == self.config.profile.guest_image_id,
            "delivery adapter uses another guest image"
        );
        let expected_selector = [0x73, 0xc4, 0x57, 0xba];
        for target in [adapter, verifier] {
            ensure!(
                self.call(target, abi::SELECTORCall {}).await?.as_slice() == expected_selector,
                "delivery verifier selector mismatch"
            );
            ensure!(
                self.call(target, abi::BN254_CONTROL_IDCall {}).await? == word(BN254_CONTROL_ID)?,
                "delivery verifier control id mismatch"
            );
        }
        let control_root = word(CONTROL_ROOT)?;
        ensure!(
            self.call(adapter, abi::CONTROL_ROOTCall {}).await? == control_root,
            "delivery adapter control root mismatch"
        );
        let mut first = control_root[..16].to_vec();
        first.reverse();
        let mut second = control_root[16..].to_vec();
        second.reverse();
        ensure!(
            self.call(verifier, abi::CONTROL_ROOT_0Call {})
                .await?
                .as_slice()
                == first
                && self
                    .call(verifier, abi::CONTROL_ROOT_1Call {})
                    .await?
                    .as_slice()
                    == second,
            "delivery verifier control root mismatch"
        );
        ensure!(
            self.call(asset, abi::decimalsCall {}).await? == 6,
            "local delivery asset must use six decimals"
        );
        Ok(())
    }

    async fn verify_code(&self, trust: &TrustFile, name: &str, target: Address) -> Result<()> {
        let code = hex_data(
            &self
                .rpc("eth_getCode", json!([address_hex(target), "latest"]))
                .await?,
            24_576,
        )?;
        let trusted = trust
            .contracts
            .get(name)
            .context("missing trusted delivery contract template")?;
        verify_code_template(&code, trusted)
            .with_context(|| format!("untrusted delivery {name} runtime"))
    }

    pub async fn pending_nonce(&self, wallet: Address) -> Result<u64> {
        quantity_u64(
            &self
                .rpc(
                    "eth_getTransactionCount",
                    json!([address_hex(wallet), "pending"]),
                )
                .await?,
        )
    }

    pub async fn balance(&self, wallet: Address) -> Result<String> {
        Ok(self
            .call(
                address(&self.config.asset)?,
                abi::balanceOfCall { owner: wallet },
            )
            .await?
            .to_string())
    }

    pub async fn native_balance(&self, wallet: Address) -> Result<String> {
        Ok(quantity(
            &self
                .rpc("eth_getBalance", json!([address_hex(wallet), "latest"]))
                .await?,
        )?
        .to_string())
    }

    pub async fn allowance(&self, wallet: Address) -> Result<String> {
        Ok(self
            .call(
                address(&self.config.asset)?,
                abi::allowanceCall {
                    owner: wallet,
                    spender: address(&self.config.escrow)?,
                },
            )
            .await?
            .to_string())
    }

    pub async fn budget_at(
        &self,
        position: &ChainPosition,
        id: B256,
    ) -> Result<super::budget::BudgetSnapshot> {
        let value = self
            .call_at(
                address(&self.config.escrow)?,
                abi::getBudgetCall { id },
                position,
            )
            .await?;
        super::budget::BudgetSnapshot::from_chain(&self.config, id, value, position.clone())
    }

    pub async fn order_budget_at(&self, position: &ChainPosition, id: Word) -> Result<Word> {
        Ok(self
            .call_at(
                address(&self.config.escrow)?,
                abi::orderBudgetCall { id: id.into() },
                position,
            )
            .await?
            .into())
    }

    pub async fn current_key(&self, receiver_peer: Word) -> Result<KeyRecord> {
        let position = self.confirmed_position().await?;
        let result = self
            .call_at(
                address(&self.config.escrow)?,
                abi::keysCall {
                    peer: receiver_peer.into(),
                },
                &position,
            )
            .await?;
        Ok(KeyRecord {
            binding_hash: result.bindingHash.into(),
            serial: result.serial,
            valid_from: result.validFrom,
            valid_until: result.validUntil,
            active: result.active,
        })
    }

    pub async fn order(&self, order_id: Word) -> Result<OrderState> {
        self.confirmed_order(order_id).await
    }

    pub async fn confirmed_order(&self, order_id: Word) -> Result<OrderState> {
        let position = self.confirmed_position().await?;
        let result = self.order_at(&position, order_id).await?;
        self.verify_position(&position).await?;
        Ok(result)
    }

    pub async fn order_at(&self, position: &ChainPosition, order_id: Word) -> Result<OrderState> {
        let result = self
            .call_at(
                address(&self.config.escrow)?,
                abi::getOrderCall {
                    id: order_id.into(),
                },
                position,
            )
            .await?;
        ensure!(
            result.terms.asset == address(&self.config.asset)?
                && result.terms.profileId.as_slice() == self.config.profile.digest()?,
            "order belongs to another delivery deployment"
        );
        ensure!(
            result.withdrawn <= result.earned
                && result
                    .earned
                    .checked_add(result.refunded)
                    .is_some_and(|total| total <= result.terms.escrowAmount),
            "invalid delivery chain accounting"
        );
        Ok(OrderState {
            payer: address_hex(result.terms.payer),
            provider_wallet: address_hex(result.terms.providerWallet),
            earned: result.earned.to_string(),
            withdrawn: result.withdrawn.to_string(),
            refunded: result.refunded.to_string(),
            bonus_earned: result.bonusEarned,
            bonus_refunded: result.bonusRefunded,
            windows: result
                .windows
                .into_iter()
                .map(window_state)
                .collect::<Result<_>>()?,
        })
    }

    pub async fn window(&self, order_id: Word, index: u16) -> Result<WindowState> {
        let position = self.confirmed_position().await?;
        window_state(
            self.call_at(
                address(&self.config.escrow)?,
                abi::getWindowCall {
                    id: order_id.into(),
                    index,
                },
                &position,
            )
            .await?,
        )
    }

    pub async fn public_values(
        &self,
        order_id: Word,
        index: u16,
        relation: Relation,
    ) -> Result<Statement> {
        let position = self.confirmed_position().await?;
        self.public_values_at(&position, order_id, index, relation)
            .await
    }

    pub async fn public_values_at(
        &self,
        position: &ChainPosition,
        order_id: Word,
        index: u16,
        relation: Relation,
    ) -> Result<Statement> {
        let result = self
            .call_at(
                address(&self.config.escrow)?,
                abi::publicValuesCall {
                    id: order_id.into(),
                    index,
                    relation: relation as u8,
                },
                position,
            )
            .await?;
        Statement::from_bytes(&result.abi_encode()).map_err(Into::into)
    }

    pub async fn confirmed_position(&self) -> Result<ChainPosition> {
        let latest = quantity_u64(&self.rpc("eth_blockNumber", json!([])).await?)?;
        let number = latest
            .checked_sub(self.config.confirmations - 1)
            .context("delivery chain has not reached the required confirmation depth")?;
        let block = self
            .rpc(
                "eth_getBlockByNumber",
                json!([format!("0x{number:x}"), false]),
            )
            .await?;
        ensure!(
            quantity_u64(&block["number"])? == number,
            "delivery RPC returned another block"
        );
        let block_hash = block["hash"]
            .as_str()
            .context("delivery confirmed block has no hash")?
            .to_owned();
        word(&block_hash)?;
        Ok(ChainPosition {
            block_number: number,
            block_hash,
            timestamp: quantity_u64(&block["timestamp"])?,
        })
    }

    pub async fn verify_position(&self, position: &ChainPosition) -> Result<()> {
        word(&position.block_hash)?;
        let block = self
            .rpc(
                "eth_getBlockByNumber",
                json!([format!("0x{:x}", position.block_number), false]),
            )
            .await?;
        ensure!(
            block["hash"].as_str() == Some(position.block_hash.as_str())
                && quantity_u64(&block["timestamp"])? == position.timestamp,
            "delivery chain position is no longer canonical"
        );
        let latest = quantity_u64(&self.rpc("eth_blockNumber", json!([])).await?)?;
        ensure!(
            latest
                .checked_sub(position.block_number)
                .is_some_and(|depth| depth >= self.config.confirmations - 1),
            "delivery chain position lost confirmation depth"
        );
        Ok(())
    }

    pub async fn prepare_intent(
        &self,
        id: String,
        wallet: Address,
        call: Call,
        gas_cap: String,
    ) -> Result<SpendIntent> {
        ensure!(
            !id.is_empty() && id.len() <= 192 && !id.chars().any(char::is_control),
            "invalid delivery spend intent id"
        );
        let cap = amount(&gas_cap)?;
        ensure!(cap > U256::ZERO, "delivery gas cap must be positive");
        let (target, calldata) = encode_call(&self.config, call, wallet)?;
        ensure!(
            !is_disclosure_calldata(&calldata),
            "recovery material requires a durable protected publication claim; estimation is forbidden"
        );
        let gas = quantity_u64(&self.rpc("eth_estimateGas", json!([{"from":address_hex(wallet),"to":address_hex(target),"data":data_hex(&calldata),"value":"0x0"}])).await?)?;
        let gas_limit = gas
            .checked_add(gas / 5)
            .and_then(|value| value.checked_add(10_000))
            .context("delivery gas estimate overflow")?;
        ensure!(
            (21_000..=MAX_GAS_LIMIT).contains(&gas_limit),
            "delivery transaction exceeds gas limit"
        );
        let price = quantity(&self.rpc("eth_gasPrice", json!([])).await?)?;
        let priority = quantity(&self.rpc("eth_maxPriorityFeePerGas", json!([])).await?)?;
        let max_fee = price
            .checked_mul(U256::from(2))
            .and_then(|value| value.checked_add(priority))
            .context("delivery gas price overflow")?;
        let worst_cost = max_fee
            .checked_mul(U256::from(gas_limit))
            .context("delivery gas cost overflow")?;
        ensure!(
            priority <= max_fee && worst_cost <= cap,
            "delivery transaction exceeds its authorized gas cap"
        );
        let intent = SpendIntent {
            id,
            chain_id: self.config.chain_id,
            wallet: address_hex(wallet),
            to: address_hex(target),
            calldata: data_hex(&calldata),
            value: "0".into(),
            gas_limit,
            gas_cap,
            max_fee_per_gas: max_fee.to_string(),
            max_priority_fee_per_gas: priority.to_string(),
        };
        checked_transaction(&self.config, &intent, wallet, 0)?;
        Ok(intent)
    }

    pub(super) fn prepare_publication_intent(
        &self,
        id: String,
        wallet: Address,
        action: super::budget::BudgetAction,
        gas_cap: String,
        policy: &super::budget::PublicationPolicy,
    ) -> Result<SpendIntent> {
        policy.validate()?;
        let (target, calldata) = action.encode(&self.config, wallet)?;
        ensure!(
            calldata.starts_with(&abi::revealBatchCall::SELECTOR),
            "offline publication only signs a bounded reveal batch"
        );
        let intent = SpendIntent {
            id,
            chain_id: self.config.chain_id,
            wallet: address_hex(wallet),
            to: address_hex(target),
            calldata: data_hex(&calldata),
            value: "0".into(),
            gas_limit: super::budget::REVEAL_GAS_LIMIT,
            gas_cap,
            max_fee_per_gas: policy.max_fee_per_gas.clone(),
            max_priority_fee_per_gas: policy.priority_fee_per_gas.clone(),
        };
        checked_transaction(&self.config, &intent, wallet, 0)?;
        Ok(intent)
    }

    pub(super) async fn publication_clock(&self) -> Result<(u64, U256)> {
        let block = self
            .rpc("eth_getBlockByNumber", json!(["latest", false]))
            .await?;
        Ok((
            quantity_u64(&block["timestamp"])?,
            quantity(&block["baseFeePerGas"])?,
        ))
    }

    pub(super) async fn latest_nonce(&self, wallet: Address) -> Result<u64> {
        quantity_u64(
            &self
                .rpc(
                    "eth_getTransactionCount",
                    json!([address_hex(wallet), "latest"]),
                )
                .await?,
        )
    }

    pub(super) async fn broadcast(&self, transaction: &SignedBytes) -> Result<String> {
        let raw = hex_data(
            &Value::String(transaction.raw_tx.clone()),
            MAX_CALLDATA_BYTES + 1024,
        )?;
        ensure!(
            !raw.is_empty() && raw[0] == 2 && word(&transaction.tx_hash)? == keccak256(&raw),
            "invalid signed delivery transaction"
        );
        let result = self
            .rpc("eth_sendRawTransaction", json!([transaction.raw_tx]))
            .await?;
        ensure!(
            word(
                result
                    .as_str()
                    .context("invalid delivery broadcast result")?
            )? == word(&transaction.tx_hash)?,
            "delivery RPC returned another transaction hash"
        );
        Ok(transaction.tx_hash.clone())
    }

    pub async fn execute(
        &self,
        delivery: &super::Delivery,
        id: &str,
        call: Call,
        gas_cap: &str,
    ) -> Result<TxObservation> {
        let existing = delivery.with_store(|store| store.find_transaction(id))?;
        let wallet = if let Some(transaction) = &existing {
            address(&transaction.intent.wallet)?
        } else {
            let wallet = delivery
                .wallet
                .lock()
                .map_err(|_| anyhow::anyhow!("delivery wallet lock failed"))?;
            ensure!(
                matches!(
                    wallet.status(),
                    super::wallet::WalletStatus::Unlocked { .. }
                ),
                "unlock the local delivery wallet before reserving a new spend"
            );
            wallet.address()?
        };
        let transaction = if let Some(transaction) = existing {
            let (target, calldata) = encode_call(&self.config, call, wallet)?;
            ensure!(
                transaction.intent.to == address_hex(target)
                    && transaction.intent.calldata == data_hex(&calldata)
                    && transaction.intent.wallet == address_hex(wallet)
                    && transaction.intent.gas_cap == gas_cap,
                "delivery spend intent changed; reconcile the original authorization"
            );
            transaction
        } else {
            delivery.authorize_spend(id, &self.config).await?;
            let intent = self
                .prepare_intent(id.to_owned(), wallet, call, gas_cap.to_owned())
                .await?;
            let pending_nonce = self.pending_nonce(wallet).await?;
            delivery.authorize_spend(id, &self.config).await?;
            let group = id
                .split('/')
                .next()
                .context("delivery intent has no budget group")?;
            ensure!(
                !group.is_empty()
                    && group
                        .bytes()
                        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_')),
                "invalid delivery transaction budget group"
            );
            delivery.with_store(|store| {
                store.reserve_budget_transaction(&intent, pending_nonce, group, gas_cap)
            })?
        };
        if transaction.raw_tx.is_none() {
            delivery.authorize_spend(id, &self.config).await?;
            let signed = delivery
                .wallet
                .lock()
                .map_err(|_| anyhow::anyhow!("delivery wallet lock failed"))?
                .sign_transaction(&self.config, &transaction.intent, transaction.nonce)?;
            delivery
                .with_store(|store| store.sign_transaction(id, &signed.raw_tx, &signed.tx_hash))?;
        }
        self.reconcile_transaction(delivery, id).await
    }

    pub(super) async fn reconcile_transaction(
        &self,
        delivery: &super::Delivery,
        id: &str,
    ) -> Result<TxObservation> {
        let transaction = delivery.with_store(|store| store.transaction(id))?;
        ensure!(
            !is_disclosure_calldata(&hex_data(
                &Value::String(transaction.intent.calldata.clone()),
                MAX_CALLDATA_BYTES
            )?),
            "protected publication attempts are reconciled only through their durable claim"
        );
        let wallet = address(&transaction.intent.wallet)?;
        checked_transaction(&self.config, &transaction.intent, wallet, transaction.nonce)?;
        let mut result = self.observe(&transaction).await.unwrap_or_else(|_| {
            observation(
                "unknown",
                transaction.block_number,
                transaction.block_hash.clone(),
                Some(
                    "delivery chain observation failed; the original transaction remains reserved",
                ),
            )
        });
        if matches!(result.phase.as_str(), "unknown" | "reorged")
            && !matches!(transaction.phase.as_str(), "conflicted" | "reverted")
        {
            let pending_nonce = self.pending_nonce(wallet).await;
            let nonce_available =
                matches!(&pending_nonce, Ok(nonce) if *nonce <= transaction.nonce);
            let authorized = delivery.authorize_spend(id, &self.config).await.is_ok();
            let unlocked = matches!(delivery.wallet.lock().map_err(|_| anyhow::anyhow!("delivery wallet lock failed"))?.status(), super::wallet::WalletStatus::Unlocked { address } if address == transaction.intent.wallet);
            if !authorized || !unlocked || !nonce_available {
                result.phase = "unknown".into();
                result.tx_hash = transaction.tx_hash;
                result.error = Some(if !authorized || !unlocked {
                    "delivery wallet is locked or authorization changed; signed transaction was preserved for reconciliation"
                } else if pending_nonce.is_err() {
                    "delivery nonce observation failed; signed transaction was preserved for reconciliation"
                } else {
                    "wallet nonce advanced but the original transaction outcome remains unproven; reconcile its original hash"
                }.into());
                delivery.with_store(|store| {
                    store.observe_transaction(
                        id,
                        &result.phase,
                        result.block_number,
                        result.block_hash.as_deref(),
                        result.error.as_deref(),
                    )
                })?;
                return Ok(result);
            }
            let signed = SignedBytes {
                raw_tx: transaction
                    .raw_tx
                    .clone()
                    .context("delivery raw transaction is missing")?,
                tx_hash: transaction
                    .tx_hash
                    .clone()
                    .context("delivery transaction hash is missing")?,
            };
            result = match self.broadcast(&signed).await {
                Ok(_) => self.observe(&transaction).await.unwrap_or_else(|_| {
                    observation(
                        "unknown",
                        None,
                        None,
                        Some("delivery broadcast outcome requires reconciliation"),
                    )
                }),
                Err(_) => observation(
                    "unknown",
                    result.block_number,
                    result.block_hash,
                    Some("delivery broadcast outcome requires reconciliation"),
                ),
            };
        }
        result.tx_hash = transaction.tx_hash;
        delivery.with_store(|store| {
            store.observe_transaction(
                id,
                &result.phase,
                result.block_number,
                result.block_hash.as_deref(),
                result.error.as_deref(),
            )
        })?;
        Ok(result)
    }

    pub async fn observe(&self, transaction: &SignedTransaction) -> Result<TxObservation> {
        let mut result = self.observe_inner(transaction).await?;
        result.tx_hash = transaction.tx_hash.clone();
        Ok(result)
    }

    async fn observe_inner(&self, transaction: &SignedTransaction) -> Result<TxObservation> {
        let expected = transaction
            .tx_hash
            .as_deref()
            .context("delivery transaction has not been signed")?;
        word(expected)?;
        let receipt = self
            .rpc("eth_getTransactionReceipt", json!([expected]))
            .await?;
        if !receipt.is_null() {
            ensure!(
                receipt["transactionHash"].as_str() == Some(expected),
                "delivery receipt returned another transaction"
            );
            ensure!(
                address(receipt["from"].as_str().context("receipt has no sender")?)?
                    == address(&transaction.intent.wallet)?,
                "delivery receipt sender mismatch"
            );
            ensure!(
                address(receipt["to"].as_str().context("receipt has no recipient")?)?
                    == address(&transaction.intent.to)?,
                "delivery receipt recipient mismatch"
            );
            let block_number = quantity_u64(&receipt["blockNumber"])?;
            let block_hash = receipt["blockHash"]
                .as_str()
                .context("receipt has no block hash")?
                .to_owned();
            word(&block_hash)?;
            let status = quantity_u64(&receipt["status"])?;
            ensure!(status <= 1, "invalid delivery transaction receipt status");
            let canonical = self
                .rpc(
                    "eth_getBlockByNumber",
                    json!([format!("0x{block_number:x}"), false]),
                )
                .await?;
            if canonical["hash"].as_str() != Some(block_hash.as_str()) {
                return Ok(observation(
                    "reorged",
                    None,
                    None,
                    Some("delivery receipt is no longer canonical"),
                ));
            }
            let latest = quantity_u64(&self.rpc("eth_blockNumber", json!([])).await?)?;
            return receipt_observation(
                block_number,
                block_hash,
                status == 1,
                latest,
                self.config.confirmations,
            );
        }
        if let (Some(number), Some(hash)) = (transaction.block_number, &transaction.block_hash) {
            let block = self
                .rpc(
                    "eth_getBlockByNumber",
                    json!([format!("0x{number:x}"), false]),
                )
                .await?;
            if block["hash"].as_str() == Some(hash.as_str()) {
                return Ok(observation(
                    "unknown",
                    Some(number),
                    Some(hash.clone()),
                    Some("delivery receipt is temporarily unavailable at its observed block"),
                ));
            }
            return Ok(observation(
                "reorged",
                None,
                None,
                Some("delivery transaction left its observed block"),
            ));
        }
        if transaction.phase == "reorged" {
            return Ok(observation(
                "reorged",
                None,
                None,
                Some("delivery transaction requires confirmation on the current chain"),
            ));
        }
        let known = self
            .rpc("eth_getTransactionByHash", json!([expected]))
            .await?;
        if !known.is_null() {
            ensure!(
                known["hash"].as_str() == Some(expected)
                    && quantity_u64(&known["nonce"])? == transaction.nonce
                    && address(
                        known["from"]
                            .as_str()
                            .context("transaction has no sender")?
                    )? == address(&transaction.intent.wallet)?
                    && address(
                        known["to"]
                            .as_str()
                            .context("transaction has no recipient")?
                    )? == address(&transaction.intent.to)?
                    && known["input"].as_str() == Some(transaction.intent.calldata.as_str()),
                "delivery RPC returned a conflicting transaction"
            );
            return Ok(observation("submitted", None, None, None));
        }
        let nonce = self
            .pending_nonce(address(&transaction.intent.wallet)?)
            .await?;
        Ok(missing_receipt_observation(transaction.nonce, nonce))
    }

    async fn call<C: SolCall>(&self, target: Address, call: C) -> Result<C::Return> {
        let result = self
            .rpc(
                "eth_call",
                json!([{"to":address_hex(target),"data":data_hex(&call.abi_encode())}, "latest"]),
            )
            .await?;
        C::abi_decode_returns_validate(&hex_data(&result, MAX_RPC_BYTES / 2)?).map_err(Into::into)
    }

    async fn call_at<C: SolCall>(
        &self,
        target: Address,
        call: C,
        position: &ChainPosition,
    ) -> Result<C::Return> {
        self.verify_position(position).await?;
        let result = self.rpc("eth_call", json!([{"to":address_hex(target),"data":data_hex(&call.abi_encode())}, format!("0x{:x}", position.block_number)])).await?;
        let decoded = C::abi_decode_returns_validate(&hex_data(&result, MAX_RPC_BYTES / 2)?)?;
        self.verify_position(position).await?;
        Ok(decoded)
    }

    async fn rpc(&self, method: &str, params: Value) -> Result<Value> {
        if matches!(method, "eth_estimateGas" | "eth_call" | "debug_traceCall")
            && let Some(data) = params
                .get(0)
                .and_then(|call| call.get("data"))
                .and_then(Value::as_str)
        {
            ensure!(
                !is_disclosure_calldata(&hex_data(
                    &Value::String(data.to_owned()),
                    MAX_CALLDATA_BYTES
                )?),
                "recovery material must never be sent to an estimate, call or simulation"
            );
        }
        let id = self.request_id.fetch_add(1, Ordering::Relaxed);
        let mut response = self
            .client
            .post(&self.config.rpc_url)
            .json(&json!({"jsonrpc":"2.0","id":id,"method":method,"params":params}))
            .send()
            .await?
            .error_for_status()?;
        ensure!(
            response
                .content_length()
                .is_none_or(|length| length <= MAX_RPC_BYTES as u64),
            "delivery RPC response exceeds limit"
        );
        let mut bytes = Vec::new();
        while let Some(chunk) = response.chunk().await? {
            ensure!(
                bytes
                    .len()
                    .checked_add(chunk.len())
                    .is_some_and(|size| size <= MAX_RPC_BYTES),
                "delivery RPC response exceeds limit"
            );
            bytes.extend_from_slice(&chunk);
        }
        let value: Value = serde_json::from_slice(&bytes)?;
        ensure!(
            value["jsonrpc"] == "2.0" && value["id"] == id,
            "invalid delivery RPC response envelope"
        );
        ensure!(
            value.get("error").is_none(),
            "delivery chain rejected {method}"
        );
        value
            .get("result")
            .cloned()
            .context("delivery RPC response has no result")
    }
}

fn encode_call(config: &ChainConfig, call: Call, wallet: Address) -> Result<(Address, Vec<u8>)> {
    config.validate()?;
    let escrow = address(&config.escrow)?;
    let calldata = match call {
        Call::Budget(action) => return action.encode(config, wallet),
        Call::Approve { amount } => {
            return Ok((
                address(&config.asset)?,
                abi::approveCall {
                    spender: escrow,
                    amount: U256::from_be_bytes(amount.to_be_bytes()),
                }
                .abi_encode(),
            ));
        }
        Call::RegisterKey { statement, seal } => {
            ensure!(
                statement.relation()? == Relation::KeyBinding,
                "register key requires its own delivery relation"
            );
            let input = abi::DeliveryStatement::abi_decode_validate(statement.abi_bytes())?;
            validate_statement(config, &input)?;
            validate_seal(&seal)?;
            abi::registerKeyCall {
                input,
                seal: seal.into(),
            }
            .abi_encode()
        }
        Call::Open {
            order_id,
            index,
            seal,
        } => {
            validate_order_index(order_id, index)?;
            validate_seal(&seal)?;
            abi::openCall {
                id: order_id.into(),
                index,
                seal: seal.into(),
            }
            .abi_encode()
        }
    };
    ensure!(
        calldata.len() <= MAX_CALLDATA_BYTES,
        "delivery calldata exceeds its limit"
    );
    Ok((escrow, calldata))
}

pub(super) fn validate_statement(
    config: &ChainConfig,
    input: &abi::DeliveryStatement,
) -> Result<()> {
    Statement::from_bytes(&input.abi_encode())?;
    ensure!(
        input.chainId == U256::from(config.chain_id)
            && input.escrow == address(&config.escrow)?
            && input.profileId.as_slice() == config.profile.digest()?
            && input.guestImageId.as_slice() == config.profile.guest_image_id,
        "delivery statement belongs to another deployment"
    );
    Ok(())
}

pub(super) fn checked_transaction(
    config: &ChainConfig,
    intent: &SpendIntent,
    wallet: Address,
    nonce: u64,
) -> Result<TxEip1559> {
    config.validate()?;
    ensure!(
        intent.chain_id == config.chain_id
            && address(&intent.wallet)? == wallet
            && amount(&intent.value)? == U256::ZERO,
        "delivery transaction has a different chain, wallet or native value"
    );
    ensure!(
        (21_000..=MAX_GAS_LIMIT).contains(&intent.gas_limit),
        "delivery transaction gas limit is invalid"
    );
    let max_fee = amount(&intent.max_fee_per_gas)?;
    let priority_fee = amount(&intent.max_priority_fee_per_gas)?;
    let gas_cap = amount(&intent.gas_cap)?;
    ensure!(
        max_fee > U256::ZERO
            && priority_fee <= max_fee
            && max_fee
                .checked_mul(U256::from(intent.gas_limit))
                .is_some_and(|cost| cost <= gas_cap),
        "delivery transaction exceeds its gas authorization"
    );
    let target = address(&intent.to)?;
    let input = hex_data(&Value::String(intent.calldata.clone()), MAX_CALLDATA_BYTES)?;
    validate_calldata(config, target, &input, wallet)?;
    Ok(TxEip1559 {
        chain_id: config.chain_id,
        nonce,
        gas_limit: intent.gas_limit,
        max_fee_per_gas: u128::try_from(max_fee).context("delivery max fee exceeds uint128")?,
        max_priority_fee_per_gas: u128::try_from(priority_fee)
            .context("delivery priority fee exceeds uint128")?,
        to: TxKind::Call(target),
        value: U256::ZERO,
        input: input.into(),
        access_list: Default::default(),
    })
}

fn validate_calldata(
    config: &ChainConfig,
    target: Address,
    input: &[u8],
    wallet: Address,
) -> Result<()> {
    let selector = input
        .get(..4)
        .context("delivery transaction calldata is missing its selector")?;
    if target == address(&config.asset)? {
        ensure!(
            selector == abi::approveCall::SELECTOR,
            "delivery wallet only approves the fixed asset"
        );
        let call = abi::approveCall::abi_decode_validate(input)?;
        ensure!(
            call.spender == address(&config.escrow)?
                && call.amount != U256::MAX
                && call.abi_encode() == input,
            "delivery approval must name the fixed escrow and an exact amount"
        );
        return Ok(());
    }
    ensure!(
        target == address(&config.escrow)?,
        "delivery wallet refuses an unknown recipient"
    );
    macro_rules! exact {
        ($name:ident) => {{
            let call = abi::$name::abi_decode_validate(input)?;
            ensure!(call.abi_encode() == input, "noncanonical delivery calldata");
            call
        }};
    }
    match selector {
        value if value == abi::registerKeyCall::SELECTOR => {
            let call = exact!(registerKeyCall);
            validate_statement(config, &call.input)?;
            ensure!(call.input.relation == 1, "invalid delivery key relation");
            validate_seal(&call.seal)?;
        }
        value if value == abi::openCall::SELECTOR => {
            let call = exact!(openCall);
            validate_order_index(call.id.into(), call.index)?;
            validate_seal(&call.seal)?;
        }
        _ => return super::budget::validate_calldata(config, input, wallet),
    }
    Ok(())
}

pub fn quote_wallet_digest(
    config: &ChainConfig,
    quote_digest: Word,
    provider_peer: Word,
) -> Result<B256> {
    config.validate()?;
    let domain = eip712_domain! { name: "VibefaceDelivery", version: "1", chain_id: config.chain_id, verifying_contract: address(&config.escrow)?, };
    Ok(abi::DeliveryQuote {
        quoteDigest: quote_digest.into(),
        providerPeer: provider_peer.into(),
    }
    .eip712_signing_hash(&domain))
}

pub(super) fn validate_order_index(order_id: Word, index: u16) -> Result<()> {
    ensure!(
        order_id != [0; 32] && index < 16,
        "invalid delivery order or window index"
    );
    Ok(())
}
pub(super) fn validate_seal(seal: &[u8]) -> Result<()> {
    ensure!(
        seal.len() == 260 && seal[..4] == [0x73, 0xc4, 0x57, 0xba],
        "delivery proof must use the pinned Groth16 selector and size"
    );
    Ok(())
}

fn is_disclosure_calldata(input: &[u8]) -> bool {
    input.starts_with(&abi::revealBatchCall::SELECTOR)
}
fn window_state(window: abi::ChainWindow) -> Result<WindowState> {
    ensure!(
        window.phase <= 4 && (window.capsule.is_empty() || window.capsule.len() == 80),
        "invalid delivery window state"
    );
    if !window.capsule.is_empty() {
        ensure!(
            vibeface_delivery::hash(&window.capsule).as_slice()
                == window.spec.capsuleHash.as_slice(),
            "delivery capsule does not match its source commitment"
        );
    }
    Ok(WindowState {
        phase: window.phase,
        decryption_proven: window.decryptionProven,
        capsule: window.capsule.to_vec(),
        ciphertext_hash: window.spec.ciphertextHash.into(),
        capsule_hash: window.spec.capsuleHash.into(),
    })
}
fn observation(
    phase: &str,
    block_number: Option<u64>,
    block_hash: Option<String>,
    error: Option<&str>,
) -> TxObservation {
    TxObservation {
        phase: phase.into(),
        tx_hash: None,
        block_number,
        block_hash,
        error: error.map(str::to_owned),
    }
}
fn receipt_observation(
    block_number: u64,
    block_hash: String,
    succeeded: bool,
    latest: u64,
    confirmations: u64,
) -> Result<TxObservation> {
    ensure!(
        confirmations > 0 && latest >= block_number,
        "delivery chain head precedes its receipt"
    );
    let confirmed = latest - block_number >= confirmations - 1;
    Ok(observation(
        if confirmed {
            if succeeded { "confirmed" } else { "reverted" }
        } else {
            "confirming"
        },
        Some(block_number),
        Some(block_hash),
        (!succeeded).then_some("delivery chain transaction reverted"),
    ))
}
fn missing_receipt_observation(original_nonce: u64, pending_nonce: u64) -> TxObservation {
    observation(
        "unknown",
        None,
        None,
        Some(if pending_nonce > original_nonce {
            "wallet nonce advanced but the original transaction outcome remains unproven; reconcile its original hash"
        } else {
            "delivery transaction has no canonical receipt; reconcile the same signed transaction"
        }),
    )
}

pub fn address(value: &str) -> Result<Address> {
    ensure!(
        value.len() == 42
            && value.starts_with("0x")
            && value[2..]
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase()),
        "invalid canonical delivery wallet address"
    );
    Ok(Address::from_slice(&hex::decode(&value[2..])?))
}
pub fn address_hex(value: Address) -> String {
    format!("0x{}", hex::encode(value))
}
fn word(value: &str) -> Result<B256> {
    ensure!(
        value.len() == 66 && value.starts_with("0x"),
        "invalid delivery bytes32"
    );
    let bytes = hex_data(&Value::String(value.to_owned()), 32)?;
    ensure!(bytes.len() == 32, "invalid delivery bytes32 length");
    Ok(B256::from_slice(&bytes))
}
pub(super) fn amount(value: &str) -> Result<U256> {
    Ok(U256::from_be_bytes(
        Amount::from_decimal_str(value)?.to_be_bytes(),
    ))
}
fn quantity(value: &Value) -> Result<U256> {
    let value = value
        .as_str()
        .context("delivery RPC quantity is not a string")?;
    let digits = value
        .strip_prefix("0x")
        .context("delivery RPC quantity lacks its hex prefix")?;
    ensure!(
        !digits.is_empty()
            && digits.len() <= 64
            && (digits.len() == 1 || !digits.starts_with('0'))
            && digits
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase()),
        "invalid canonical delivery RPC quantity"
    );
    U256::from_str_radix(digits, 16).map_err(Into::into)
}
fn quantity_u64(value: &Value) -> Result<u64> {
    u64::try_from(quantity(value)?).context("delivery RPC quantity exceeds uint64")
}
fn data_hex(value: &[u8]) -> String {
    format!("0x{}", hex::encode(value))
}
fn hex_data(value: &Value, limit: usize) -> Result<Vec<u8>> {
    let value = value
        .as_str()
        .context("delivery RPC data is not a string")?;
    let digits = value
        .strip_prefix("0x")
        .context("delivery RPC data lacks its hex prefix")?;
    ensure!(
        digits.len() % 2 == 0
            && digits.len()
                <= limit
                    .checked_mul(2)
                    .context("delivery data limit overflow")?
            && digits
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase()),
        "invalid or oversized delivery RPC data"
    );
    hex::decode(digits).map_err(Into::into)
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct TrustFile {
    schema_version: u32,
    compiler: String,
    statement_schema_hash: String,
    contracts: BTreeMap<String, CodeTemplate>,
}
#[derive(Deserialize)]
struct CodeTemplate {
    runtime: String,
    immutable_ranges: Vec<ImmutableRange>,
    immutable_groups: Vec<Vec<ImmutableRange>>,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Deserialize)]
struct ImmutableRange {
    start: usize,
    length: usize,
}

fn verify_code_template(actual: &[u8], template: &CodeTemplate) -> Result<()> {
    let mut expected = hex_data(&Value::String(template.runtime.clone()), 24_576)?;
    ensure!(
        !expected.is_empty() && actual.len() == expected.len(),
        "delivery runtime size mismatch"
    );
    let mut masked = actual.to_vec();
    let mut groups_flat = Vec::new();
    for group in &template.immutable_groups {
        ensure!(!group.is_empty(), "empty trusted immutable group");
        let mut common: Option<&[u8]> = None;
        for range in group {
            let end = range
                .start
                .checked_add(range.length)
                .context("immutable range overflow")?;
            ensure!(
                range.length == 32 && end <= actual.len(),
                "invalid trusted immutable range"
            );
            let value = &actual[range.start..end];
            ensure!(
                common.is_none_or(|first| first == value),
                "delivery runtime immutable references disagree"
            );
            common = Some(value);
            groups_flat.push(*range);
        }
    }
    groups_flat.sort();
    let mut declared = template.immutable_ranges.clone();
    declared.sort();
    ensure!(
        groups_flat == declared,
        "delivery immutable groups differ from the trusted ranges"
    );
    let mut previous_end = 0;
    for range in declared {
        let end = range
            .start
            .checked_add(range.length)
            .context("immutable range overflow")?;
        ensure!(
            range.start >= previous_end && end <= expected.len() && range.length == 32,
            "overlapping or invalid trusted immutable ranges"
        );
        ensure!(
            expected[range.start..end].iter().all(|byte| *byte == 0),
            "trusted immutable template is not blank"
        );
        expected[range.start..end].fill(0);
        masked[range.start..end].fill(0);
        previous_end = end;
    }
    ensure!(
        masked == expected,
        "delivery runtime code differs from the fixed trusted template"
    );
    Ok(())
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;

    pub struct RpcFixture {
        pub chain: Chain,
        pub requests: std::sync::Arc<std::sync::Mutex<Vec<Value>>>,
        task: tokio::task::JoinHandle<()>,
    }
    impl Drop for RpcFixture {
        fn drop(&mut self) {
            self.task.abort();
        }
    }
    pub async fn rpc_fixture(
        mut config: ChainConfig,
        response: impl Fn(&Value) -> Value + Send + Sync + 'static,
    ) -> Result<RpcFixture> {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        config.rpc_url = format!("http://{}/", listener.local_addr()?);
        let requests = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let captured = requests.clone();
        let response = std::sync::Arc::new(response);
        let router = axum::Router::new().route(
            "/",
            axum::routing::post(move |axum::Json(request): axum::Json<Value>| {
                let captured = captured.clone();
                let response = response.clone();
                async move {
                    captured.lock().unwrap().push(request.clone());
                    axum::Json(
                        json!({"jsonrpc":"2.0","id":request["id"],"result":response(&request)}),
                    )
                }
            }),
        );
        let task = tokio::spawn(async move {
            let _ = axum::serve(listener, router).await;
        });
        Ok(RpcFixture {
            chain: Chain {
                config,
                client: reqwest::Client::new(),
                request_id: AtomicU64::new(1),
            },
            requests,
            task,
        })
    }

    pub fn config() -> ChainConfig {
        ChainConfig {
            rpc_url: "http://127.0.0.1:8545".into(),
            chain_id: LOCAL_CHAIN_ID,
            escrow: "0x1111111111111111111111111111111111111111".into(),
            asset: "0x2222222222222222222222222222222222222222".into(),
            confirmations: 2,
            profile: serde_json::from_str(include_str!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/../../crates/delivery/proof/profile.json"
            )))
            .expect("built-in delivery profile"),
        }
    }
    #[test]
    fn a_missing_receipt_and_advanced_pending_nonce_never_prove_payment_failure() {
        for pending in [6, 7, 8, 100] {
            let observation = missing_receipt_observation(7, pending);
            assert_eq!(observation.phase, "unknown");
            assert!(observation.block_number.is_none());
            assert!(observation.block_hash.is_none());
        }
    }
    #[test]
    fn configuration_rejects_another_valid_image_or_proof_budget() {
        let mut other = config();
        assert!(other.validate().is_ok());
        other.profile.guest_image_id[0] ^= 1;
        assert!(other.profile.validate().is_ok());
        assert!(other.validate().is_err());
        let mut config = config();
        config.profile.max_windows = 1;
        config.profile.max_total_bytes = 65536;
        assert!(config.profile.validate().is_ok());
        assert!(config.validate().is_err());
    }

    pub fn approval_intent(wallet: Address) -> Result<SpendIntent> {
        let config = config();
        let (target, calldata) = encode_call(
            &config,
            Call::Approve {
                amount: Amount::from_u64(100),
            },
            wallet,
        )?;
        Ok(SpendIntent {
            id: "synthetic-intent".into(),
            chain_id: LOCAL_CHAIN_ID,
            wallet: address_hex(wallet),
            to: address_hex(target),
            calldata: data_hex(&calldata),
            value: "0".into(),
            gas_limit: 50_000,
            gas_cap: "100000000000000".into(),
            max_fee_per_gas: "1000000000".into(),
            max_priority_fee_per_gas: "1".into(),
        })
    }

    #[tokio::test]
    async fn changing_deployment_during_nonce_or_receipt_wait_stops_signing_and_rebroadcast()
    -> Result<()> {
        use super::super::wallet::SecretString;
        use super::super::{
            Delivery, DeliveryConfig,
            budget::{BudgetSnapshot, BudgetTerms},
        };
        use std::sync::{Arc, atomic::AtomicBool};
        let dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../.artifacts")
            .join(format!("chain-configuration-{}", uuid::Uuid::new_v4()));
        crate::config::private_dir(&dir)?;
        let delivery = Arc::new(Delivery::open(
            &dir,
            crate::crypto::LocalIdentity::generate(),
        )?);
        let password = SecretString::new("synthetic configuration regression wallet".into());
        let wallet = {
            let mut wallet = delivery.wallet.lock().unwrap();
            wallet.create(&password)?;
            wallet.unlock(&password)?;
            wallet.address()?
        };
        let on_nonce = Arc::new(AtomicBool::new(true));
        let on_receipt = Arc::new(AtomicBool::new(false));
        let captured_delivery = delivery.clone();
        let nonce_switch = on_nonce.clone();
        let receipt_switch = on_receipt.clone();
        let rpc = rpc_fixture(config(), move |request| {
            let method = request["method"].as_str().unwrap();
            if method == "eth_getTransactionCount" && nonce_switch.swap(false, Ordering::SeqCst)
                || method == "eth_getTransactionReceipt"
                    && receipt_switch.swap(false, Ordering::SeqCst)
            {
                let mut changed = captured_delivery.config();
                changed.chain.as_mut().unwrap().rpc_url = "http://127.0.0.1:9/".into();
                captured_delivery.configure_local(changed).unwrap();
            }
            match method {
                "eth_estimateGas" => json!("0x5208"),
                "eth_gasPrice" | "eth_maxPriorityFeePerGas" => json!("0x1"),
                "eth_getTransactionCount" => json!("0x0"),
                _ => Value::Null,
            }
        })
        .await?;
        let settings = DeliveryConfig {
            enabled: true,
            paid_enabled: true,
            chain: Some(rpc.chain.config.clone()),
            ..Default::default()
        };
        delivery.configure_local(settings.clone())?;
        let budget_id = "44".repeat(32);
        let cap = "100000000000000";
        delivery.register_budget(
            &BudgetSnapshot {
                terms: BudgetTerms {
                    budget_id: budget_id.clone(),
                    chain_id: rpc.chain.config.chain_id,
                    escrow: rpc.chain.config.escrow.clone(),
                    asset: rpc.chain.config.asset.clone(),
                    payer: address_hex(wallet),
                    payee: address_hex(Address::repeat_byte(3)),
                    provider_peer: "55".repeat(32),
                    deposit: "100".into(),
                },
                locked: "0".into(),
                earned: "0".into(),
                withdrawn: "0".into(),
                refunded: "0".into(),
                free: "100".into(),
                max_claim_until: 0,
                state: "open".into(),
                position: ChainPosition {
                    block_number: 1,
                    block_hash: data_hex(&[0x55; 32]),
                    timestamp: 1,
                },
            },
            cap,
        )?;
        let fresh_id = format!("budget-{budget_id}/fresh");
        let resume_id = format!("budget-{budget_id}/resume");
        for id in [&fresh_id, &resume_id] {
            delivery.with_store(|s| s.put_value(&format!("local-spend:{id}"), b"owner"))?;
        }
        assert!(
            rpc.chain
                .execute(
                    &delivery,
                    &fresh_id,
                    Call::Approve {
                        amount: Amount::from_u64(100)
                    },
                    cap
                )
                .await
                .is_err()
        );
        assert!(
            delivery
                .with_store(|s| s.find_transaction(&fresh_id))?
                .is_none(),
            "a changed route cannot reserve or sign a new transaction"
        );
        delivery.configure_local(settings)?;
        let mut intent = approval_intent(wallet)?;
        intent.id = resume_id.clone();
        let reserved = delivery.with_store(|s| {
            s.reserve_budget_transaction(&intent, 0, &format!("budget-{budget_id}"), cap)
        })?;
        let signed = delivery.wallet.lock().unwrap().sign_transaction(
            rpc.chain.config(),
            &intent,
            reserved.nonce,
        )?;
        delivery.with_store(|s| s.sign_transaction(&resume_id, &signed.raw_tx, &signed.tx_hash))?;
        let before = delivery.with_store(|s| s.transaction(&resume_id))?;
        on_receipt.store(true, Ordering::SeqCst);
        assert_eq!(
            rpc.chain
                .reconcile_transaction(&delivery, &resume_id)
                .await?
                .phase,
            "unknown"
        );
        let after = delivery.with_store(|s| s.transaction(&resume_id))?;
        assert_eq!(after.intent, before.intent);
        assert_eq!(after.nonce, before.nonce);
        assert_eq!(after.raw_tx, before.raw_tx);
        assert_eq!(after.tx_hash, before.tx_hash);
        assert!(
            !rpc.requests
                .lock()
                .unwrap()
                .iter()
                .any(|request| request["method"] == "eth_sendRawTransaction"),
            "configuration changes during awaits cannot reach broadcast"
        );
        std::fs::remove_dir_all(dir)?;
        Ok(())
    }

    #[test]
    fn signer_accepts_only_fixed_actions_exact_amounts_and_bounded_gas() -> Result<()> {
        let config = config();
        let wallet = Address::repeat_byte(3);
        let intent = approval_intent(wallet)?;
        checked_transaction(&config, &intent, wallet, 7)?;
        let mut wrong = intent.clone();
        wrong.to = address_hex(Address::repeat_byte(4));
        assert!(checked_transaction(&config, &wrong, wallet, 7).is_err());
        let mut wrong = intent.clone();
        wrong.value = "1".into();
        assert!(checked_transaction(&config, &wrong, wallet, 7).is_err());
        let mut wrong = intent.clone();
        wrong.gas_cap = "1".into();
        assert!(checked_transaction(&config, &wrong, wallet, 7).is_err());
        let mut wrong = intent.clone();
        wrong.chain_id = 1;
        assert!(checked_transaction(&config, &wrong, wallet, 7).is_err());
        let mut wrong = intent.clone();
        wrong.calldata = data_hex(
            &abi::approveCall {
                spender: Address::repeat_byte(5),
                amount: U256::from(100),
            }
            .abi_encode(),
        );
        assert!(checked_transaction(&config, &wrong, wallet, 7).is_err());
        let mut wrong = intent.clone();
        wrong.calldata.push_str("00");
        assert!(checked_transaction(&config, &wrong, wallet, 7).is_err());
        let mut wrong_config = config.clone();
        wrong_config.rpc_url = "http://localhost:8545".into();
        assert!(wrong_config.validate().is_err());
        wrong_config.rpc_url = "http://127.0.0.1:8545@remote.example".into();
        assert!(wrong_config.validate().is_err());
        Ok(())
    }

    #[test]
    fn receipt_requires_its_canonical_block_and_confirmation_depth() -> Result<()> {
        let hash = "0x0123".into();
        assert_eq!(
            receipt_observation(10, hash, true, 10, 2)?.phase,
            "confirming"
        );
        assert_eq!(
            receipt_observation(10, "hash".into(), true, 11, 2)?.phase,
            "confirmed"
        );
        assert_eq!(
            receipt_observation(10, "hash".into(), false, 11, 2)?.phase,
            "reverted"
        );
        assert!(receipt_observation(10, "hash".into(), true, 9, 2).is_err());
        assert_eq!(observation("unknown", None, None, None).block_hash, None);
        Ok(())
    }

    #[test]
    fn trust_template_rejects_changed_runtime_and_inconsistent_immutable_copies() -> Result<()> {
        let template = CodeTemplate {
            runtime: data_hex(&[&[0x60][..], &[0; 64][..]].concat()),
            immutable_ranges: vec![
                ImmutableRange {
                    start: 1,
                    length: 32,
                },
                ImmutableRange {
                    start: 33,
                    length: 32,
                },
            ],
            immutable_groups: vec![vec![
                ImmutableRange {
                    start: 1,
                    length: 32,
                },
                ImmutableRange {
                    start: 33,
                    length: 32,
                },
            ]],
        };
        let mut actual = vec![0x60];
        actual.extend_from_slice(&[7; 64]);
        verify_code_template(&actual, &template)?;
        actual[33] ^= 1;
        assert!(verify_code_template(&actual, &template).is_err());
        actual[33] ^= 1;
        actual[0] ^= 1;
        assert!(verify_code_template(&actual, &template).is_err());
        Ok(())
    }

    #[test]
    fn rpc_quantities_and_decimals_reject_ambiguous_or_overflow_encodings() {
        for value in ["0x", "0x00", "0xA", "1", "-1", "0x10000000000000000"] {
            assert!(quantity_u64(&json!(value)).is_err(), "{value}");
        }
        for value in ["", "01", "+1", "1.0", "1e6"] {
            assert!(amount(value).is_err(), "{value}");
        }
        assert_eq!(quantity_u64(&json!("0x0")).unwrap(), 0);
    }

    #[tokio::test]
    async fn reveal_preparation_never_estimates_or_simulates_recovery_material() -> Result<()> {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let mut config = config();
        config.rpc_url = format!("http://{}/", listener.local_addr()?);
        let chain = Chain {
            config,
            client: reqwest::Client::new(),
            request_id: AtomicU64::new(1),
        };
        let policy = super::super::budget::PublicationPolicy {
            watchdog_seconds: 32,
            inclusion_seconds: 5,
            finality_seconds: 5,
            clock_skew_seconds: 2,
            max_fee_per_gas: "3000000000".into(),
            priority_fee_per_gas: "1000000000".into(),
        };
        let reveal = || super::super::budget::BudgetAction::Reveal {
            budget_id: [0x33; 32],
            items: vec![super::super::budget::Reveal {
                window: super::super::budget::WindowRef {
                    order_id: "44".repeat(32),
                    index: 0,
                },
                capsule: vec![0x55; 80],
            }],
        };
        let wallet = Address::repeat_byte(0x22);
        let cap = "10000000000000000";
        assert!(
            chain
                .prepare_intent(
                    "budget-test/reveal-first".into(),
                    wallet,
                    Call::Budget(Box::new(reveal())),
                    cap.into()
                )
                .await
                .is_err()
        );
        let offline = chain.prepare_publication_intent(
            "budget-test/reveal-first".into(),
            wallet,
            reveal(),
            cap.into(),
            &policy,
        )?;
        assert_eq!(offline.gas_limit, super::super::budget::REVEAL_GAS_LIMIT);
        assert!(
            offline
                .calldata
                .starts_with(&data_hex(&abi::revealBatchCall::SELECTOR))
        );
        assert!(
            chain
                .prepare_publication_intent(
                    "budget-test/reveal-first".into(),
                    wallet,
                    reveal(),
                    "1".into(),
                    &policy
                )
                .is_err()
        );
        assert!(
            tokio::time::timeout(Duration::from_millis(20), listener.accept())
                .await
                .is_err(),
            "no E-bearing preparation may contact any RPC"
        );
        Ok(())
    }
}
