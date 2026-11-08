use super::{
    Delivery,
    chain::{self, ChainConfig, abi},
    types::Mode,
    wallet::WalletStatus,
};
use alloy_primitives::{Address, B256, Signature, U256, keccak256};
use alloy_sol_types::{SolCall, SolStruct, SolValue, eip712_domain};
use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

pub const CHALLENGE_WINDOW: u64 = 3600;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChannelTerms {
    pub channel_id: String,
    pub chain_id: u64,
    pub escrow: String,
    pub payer: String,
    pub payee: String,
    pub provider_peer: String,
    pub asset: String,
    pub budget: String,
    pub challenge_window: u64,
}

#[cfg(test)]
mod tests {
    use super::*;
    use k256::ecdsa::SigningKey;

    #[test]
    fn voucher_matches_solidity_vector_and_rejects_changed_domain_amount_and_signature()
    -> Result<()> {
        let key = SigningKey::from_bytes((&[7u8; 32]).into())?;
        let mut voucher = CumulativeVoucher {
            terms: ChannelTerms {
                channel_id: "66".repeat(32),
                chain_id: 31337,
                escrow: "0x3333333333333333333333333333333333333333".into(),
                payer: chain::address_hex(Address::from_public_key(key.verifying_key())),
                payee: "0x2222222222222222222222222222222222222222".into(),
                provider_peer: "55".repeat(32),
                asset: "0x4444444444444444444444444444444444444444".into(),
                budget: "100".into(),
                challenge_window: CHALLENGE_WINDOW,
            },
            sequence: 1,
            total_amount: "7".into(),
            receipt_commitment: "77".repeat(32),
            signature: String::new(),
        };
        assert_eq!(
            hex::encode(voucher.digest()?),
            "d00a013edebccda322a7bf6b666c2122e510f5bd4b805f6ce53d71dac8713f54"
        );
        let (signature, recovery) = key.sign_prehash_recoverable(voucher.digest()?.as_slice())?;
        voucher.signature = hex::encode(
            Signature::from_bytes_and_parity(signature.to_bytes().as_slice(), recovery.is_y_odd())
                .as_bytes(),
        );
        voucher.verify()?;
        assert!(serde_json::to_vec(&voucher)?.len() < 2048);
        let mut changed = voucher.clone();
        changed.terms.escrow = changed.terms.asset.clone();
        assert!(changed.verify().is_err());
        let mut changed = voucher.clone();
        changed.terms.channel_id = "88".repeat(32);
        assert!(changed.verify().is_err());
        let mut changed = voucher.clone();
        changed.total_amount = "101".into();
        assert!(changed.verify().is_err());
        let mut changed = voucher.clone();
        changed.signature = "00".repeat(65);
        assert!(changed.verify().is_err());
        let mut changed = voucher;
        changed.total_amount = "0.1".into();
        assert!(changed.digest().is_err());
        Ok(())
    }

    #[test]
    fn channel_signer_rejects_unpinned_and_noncanonical_calls() -> Result<()> {
        let mut config = chain::tests::config();
        assert!(escrow(&config).is_err());
        config.channel_escrow = Some("0x3333333333333333333333333333333333333333".into());
        let action = ChannelAction::Approve {
            budget: "100".into(),
        };
        let (target, input) = action.encode(&config, Address::ZERO)?;
        assert_eq!(target, chain::address(&config.asset)?);
        assert!(validate_calldata(&config, &input, Address::ZERO).is_err());
        let mut unknown = abi::closeChannelCall {
            id: B256::repeat_byte(7),
        }
        .abi_encode();
        validate_calldata(&config, &unknown, Address::ZERO)?;
        unknown.push(0);
        assert!(validate_calldata(&config, &unknown, Address::ZERO).is_err());
        assert!(validate_calldata(&config, &[1, 2, 3, 4], Address::ZERO).is_err());
        Ok(())
    }

    #[test]
    fn source_fee_reservation_is_atomic_idempotent_and_global_across_channels() -> Result<()> {
        let directory = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../.artifacts/channel-tests")
            .join(uuid::Uuid::new_v4().to_string());
        let store = super::super::store::ServiceStore::open(&directory, 131_208, 128)?;
        let mut record = ChannelRecord {
            terms: ChannelTerms {
                channel_id: "66".repeat(32),
                chain_id: 31337,
                escrow: "0x3333333333333333333333333333333333333333".into(),
                payer: "0x1111111111111111111111111111111111111111".into(),
                payee: "0x2222222222222222222222222222222222222222".into(),
                provider_peer: "55".repeat(32),
                asset: "0x4444444444444444444444444444444444444444".into(),
                budget: "100".into(),
                challenge_window: CHALLENGE_WINDOW,
            },
            authorized_amount: "0".into(),
            issued_amount: "0".into(),
            issued_sequence: 0,
            latest: None,
        };
        let fee = FeeAuthorization {
            channel_id: record.terms.channel_id.clone(),
            order_id: "77".repeat(32),
            source_peer: "11".repeat(32),
            manifest_hash: "22".repeat(32),
            amount: "80".into(),
            intent_id: uuid::Uuid::new_v4().to_string(),
        };
        reserve_fee(&store, &mut record, &fee)?;
        reserve_fee(&store, &mut record, &fee)?;
        assert_eq!(record.authorized_amount, "80");
        let mut other = fee.clone();
        other.order_id = "88".repeat(32);
        other.intent_id = uuid::Uuid::new_v4().to_string();
        assert!(reserve_fee(&store, &mut record, &other).is_err());
        assert!(store.value(&fee_key(&other.order_id))?.is_none());
        assert_eq!(
            serde_json::from_slice::<ChannelRecord>(
                &store.value(&record_key(&record.terms.channel_id))?.unwrap()
            )?
            .authorized_amount,
            "80"
        );
        other.amount = "1".into();
        other.intent_id = fee.intent_id.clone();
        assert!(reserve_fee(&store, &mut record, &other).is_err());
        let mut changed = fee.clone();
        changed.channel_id = "99".repeat(32);
        assert!(reserve_fee(&store, &mut record, &changed).is_err());
        changed = fee;
        changed.amount = "81".into();
        assert!(reserve_fee(&store, &mut record, &changed).is_err());
        drop(store);
        std::fs::remove_dir_all(directory)?;
        Ok(())
    }
}

impl ChannelTerms {
    fn validate(&self) -> Result<()> {
        word(&self.channel_id)?;
        word(&self.provider_peer)?;
        let payer = chain::address(&self.payer)?;
        let payee = chain::address(&self.payee)?;
        let escrow = chain::address(&self.escrow)?;
        let asset = chain::address(&self.asset)?;
        ensure!(
            self.chain_id == 31337
                && self.challenge_window == CHALLENGE_WINDOW
                && payer != Address::ZERO
                && payee != Address::ZERO
                && payer != payee
                && payee != escrow
                && escrow != Address::ZERO
                && escrow != asset
                && chain::amount(&self.budget)? > U256::ZERO,
            "invalid local channel terms"
        );
        Ok(())
    }

    fn matches_config(&self, config: &ChainConfig) -> Result<()> {
        self.validate()?;
        ensure!(
            self.chain_id == config.chain_id
                && chain::address(&self.escrow)? == escrow(config)?
                && chain::address(&self.asset)? == chain::address(&config.asset)?,
            "channel belongs to another pinned deployment"
        );
        Ok(())
    }

    fn matches_snapshot(&self, value: &ChannelSnapshot) -> Result<()> {
        ensure!(
            self.payer == value.payer
                && self.payee == value.payee
                && self.provider_peer == value.provider_peer
                && self.budget == value.budget,
            "on-chain channel differs from immutable terms"
        );
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CumulativeVoucher {
    pub terms: ChannelTerms,
    pub sequence: u64,
    pub total_amount: String,
    pub receipt_commitment: String,
    pub signature: String,
}

impl CumulativeVoucher {
    pub(super) fn digest(&self) -> Result<B256> {
        self.terms.validate()?;
        ensure!(
            self.sequence > 0
                && chain::amount(&self.total_amount)? > U256::ZERO
                && chain::amount(&self.total_amount)? <= chain::amount(&self.terms.budget)?,
            "voucher exceeds channel budget or has no cumulative charge"
        );
        let domain = eip712_domain! { name: "VibefaceCumulativeChannel", version: "1", chain_id: self.terms.chain_id,
        verifying_contract: chain::address(&self.terms.escrow)?, };
        Ok(abi::CumulativeVoucher {
            channelId: word(&self.terms.channel_id)?,
            sequence: self.sequence,
            totalAmount: chain::amount(&self.total_amount)?,
            receiptCommitment: word(&self.receipt_commitment)?,
        }
        .eip712_signing_hash(&domain))
    }

    fn verify(&self) -> Result<()> {
        let signature = hex::decode(&self.signature)?;
        ensure!(
            signature.len() == 65 && matches!(signature[64], 27 | 28),
            "malformed channel signature"
        );
        let signature = Signature::from_raw(&signature)?;
        ensure!(
            signature.normalize_s().is_none()
                && signature.recover_address_from_prehash(&self.digest()?)?
                    == chain::address(&self.terms.payer)?,
            "channel voucher has another signer or a malleable signature"
        );
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChannelState {
    Open,
    Closing,
    Closed,
}

#[derive(Debug, Serialize)]
pub struct ChannelSnapshot {
    pub payer: String,
    pub payee: String,
    pub provider_peer: String,
    pub budget: String,
    pub redeemed: String,
    pub sequence: u64,
    pub closes_at: u64,
    pub state: ChannelState,
    pub position: chain::ChainPosition,
}

impl ChannelSnapshot {
    pub(super) fn from_chain(
        value: abi::ChainChannel,
        position: chain::ChainPosition,
    ) -> Result<Self> {
        ensure!(
            value.payer != Address::ZERO && value.redeemed <= value.budget,
            "invalid on-chain channel accounting"
        );
        let state = match value.state {
            0 => ChannelState::Open,
            1 => ChannelState::Closing,
            2 => ChannelState::Closed,
            _ => bail!("invalid on-chain channel state"),
        };
        ensure!(
            (state == ChannelState::Open) == (value.closesAt == 0),
            "invalid channel closing deadline"
        );
        Ok(Self {
            payer: chain::address_hex(value.payer),
            payee: chain::address_hex(value.payee),
            provider_peer: hex::encode(value.providerPeer),
            budget: value.budget.to_string(),
            redeemed: value.redeemed.to_string(),
            sequence: value.sequence,
            closes_at: value.closesAt,
            state,
            position,
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
pub enum ChannelAction {
    Approve { budget: String },
    Open { terms: ChannelTerms, salt: String },
    Redeem { voucher: CumulativeVoucher },
    Close { channel_id: String },
    Refund { channel_id: String },
}

impl ChannelAction {
    pub(super) fn encode(
        &self,
        config: &ChainConfig,
        wallet: Address,
    ) -> Result<(Address, Vec<u8>)> {
        let target = escrow(config)?;
        let input = match self {
            Self::Approve { budget } => {
                let budget = chain::amount(budget)?;
                ensure!(
                    budget > U256::ZERO && budget != U256::MAX,
                    "invalid exact channel approval"
                );
                return Ok((
                    chain::address(&config.asset)?,
                    abi::approveCall {
                        spender: target,
                        amount: budget,
                    }
                    .abi_encode(),
                ));
            }
            Self::Open { terms, salt } => {
                terms.matches_config(config)?;
                let salt = word(salt)?;
                ensure!(
                    word(&terms.channel_id)?
                        == keccak256((config.chain_id, target, wallet, salt).abi_encode_params()),
                    "channel id does not match payer and salt"
                );
                ensure!(
                    chain::address(&terms.payer)? == wallet,
                    "channel deposit payer differs from wallet"
                );
                abi::openChannelCall {
                    salt,
                    payee: chain::address(&terms.payee)?,
                    providerPeer: word(&terms.provider_peer)?,
                    budget: chain::amount(&terms.budget)?,
                }
                .abi_encode()
            }
            Self::Redeem { voucher } => {
                voucher.terms.matches_config(config)?;
                voucher.verify()?;
                abi::redeemChannelCall {
                    id: word(&voucher.terms.channel_id)?,
                    sequence: voucher.sequence,
                    totalAmount: chain::amount(&voucher.total_amount)?,
                    receiptCommitment: word(&voucher.receipt_commitment)?,
                    signature: hex::decode(&voucher.signature)?.into(),
                }
                .abi_encode()
            }
            Self::Close { channel_id } => abi::closeChannelCall {
                id: word(channel_id)?,
            }
            .abi_encode(),
            Self::Refund { channel_id } => abi::refundChannelCall {
                id: word(channel_id)?,
            }
            .abi_encode(),
        };
        Ok((target, input))
    }
}

pub(super) fn escrow(config: &ChainConfig) -> Result<Address> {
    chain::address(
        config
            .channel_escrow
            .as_deref()
            .context("local channel escrow is not configured")?,
    )
}

pub(super) fn validate_calldata(config: &ChainConfig, input: &[u8], wallet: Address) -> Result<()> {
    macro_rules! exact {
        ($name:ident) => {{
            let call = abi::$name::abi_decode_validate(input)?;
            ensure!(call.abi_encode() == input, "noncanonical channel calldata");
            call
        }};
    }
    let selector = input.get(..4).context("missing channel action")?;
    match selector {
        value if value == abi::openChannelCall::SELECTOR => {
            let call = exact!(openChannelCall);
            ensure!(
                call.salt != B256::ZERO
                    && call.providerPeer != B256::ZERO
                    && call.budget > U256::ZERO
                    && call.payee != Address::ZERO
                    && call.payee != wallet
                    && call.payee != escrow(config)?,
                "invalid channel deposit"
            );
        }
        value if value == abi::redeemChannelCall::SELECTOR => {
            let call = exact!(redeemChannelCall);
            ensure!(
                call.id != B256::ZERO
                    && call.sequence > 0
                    && call.totalAmount > U256::ZERO
                    && call.receiptCommitment != B256::ZERO
                    && call.signature.len() == 65,
                "invalid channel redemption"
            );
        }
        value if value == abi::closeChannelCall::SELECTOR => {
            ensure!(
                exact!(closeChannelCall).id != B256::ZERO,
                "invalid channel id"
            );
        }
        value if value == abi::refundChannelCall::SELECTOR => {
            ensure!(
                exact!(refundChannelCall).id != B256::ZERO,
                "invalid channel id"
            );
        }
        _ => bail!("wallet refuses unknown channel action"),
    }
    Ok(())
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub enum ChannelCommand {
    Open {
        intent_id: String,
        payee: String,
        provider_peer: String,
        budget: String,
        gas_cap: String,
    },
    Authorize {
        channel_id: String,
        order_id: String,
        amount: String,
        intent_id: String,
    },
    Issue {
        order_id: String,
        intent_id: String,
    },
    Export {
        channel_id: String,
    },
    Import {
        voucher: CumulativeVoucher,
    },
    Redeem {
        channel_id: String,
        intent_id: String,
        gas_cap: String,
    },
    Close {
        channel_id: String,
        intent_id: String,
        gas_cap: String,
    },
    Refund {
        channel_id: String,
        intent_id: String,
        gas_cap: String,
    },
    Status {
        channel_id: String,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ChannelRecord {
    terms: ChannelTerms,
    authorized_amount: String,
    issued_sequence: u64,
    issued_amount: String,
    latest: Option<CumulativeVoucher>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct FeeAuthorization {
    channel_id: String,
    order_id: String,
    source_peer: String,
    manifest_hash: String,
    amount: String,
    intent_id: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ChannelSpend {
    config: ChainConfig,
    owner_peer: String,
    wallet: String,
    gas_cap: String,
    action: ChannelAction,
    approval: bool,
}

impl ChannelSpend {
    fn call(&self, step: &str) -> Result<ChannelAction> {
        match (&self.action, step) {
            (ChannelAction::Open { terms, .. }, "approve") if self.approval => {
                Ok(ChannelAction::Approve {
                    budget: terms.budget.clone(),
                })
            }
            (_, "execute") => Ok(self.action.clone()),
            _ => bail!("channel transaction has no matching owner authorization"),
        }
    }
}

fn word(value: &str) -> Result<B256> {
    ensure!(
        value.len() == 64
            && value
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
        "channel identifiers must be lowercase 32-byte hex without a prefix"
    );
    let value = B256::from_slice(&hex::decode(value)?);
    ensure!(value != B256::ZERO, "zero channel identifier");
    Ok(value)
}

fn intent(value: &str) -> Result<()> {
    ensure!(
        uuid::Uuid::parse_str(value)?.to_string() == value,
        "channel intent must be a canonical UUID"
    );
    Ok(())
}

fn record_key(id: &str) -> String {
    format!("channel:{id}")
}
fn fee_key(id: &str) -> String {
    format!("channel-fee:{id}")
}
fn charge_key(id: &str) -> String {
    format!("channel-charge:{id}")
}

fn reserve_fee(
    store: &super::store::ServiceStore,
    record: &mut ChannelRecord,
    fee: &FeeAuthorization,
) -> Result<()> {
    let key = fee_key(&fee.order_id);
    if let Some(bytes) = store.value(&key)? {
        ensure!(
            serde_json::from_slice::<FeeAuthorization>(&bytes)? == *fee,
            "Source fee authorization is immutable across amount, intent and channel"
        );
        return Ok(());
    }
    let intent_key = format!("channel-fee-intent:{}", fee.intent_id);
    ensure!(
        store.value(&intent_key)?.is_none(),
        "Source fee intent already authorizes another order"
    );
    let total = chain::amount(&record.authorized_amount)?
        .checked_add(chain::amount(&fee.amount)?)
        .context("authorized fees overflow")?;
    ensure!(
        total <= chain::amount(&record.terms.budget)?,
        "Source fee authorizations exceed the reserved channel budget"
    );
    let mut updated = record.clone();
    updated.authorized_amount = total.to_string();
    store.put_values(&[
        (
            &record_key(&record.terms.channel_id),
            &serde_json::to_vec(&updated)?,
        ),
        (&key, &serde_json::to_vec(fee)?),
        (&intent_key, fee.order_id.as_bytes()),
    ])?;
    *record = updated;
    Ok(())
}

impl Delivery {
    fn channel_record(&self, id: &str) -> Result<ChannelRecord> {
        word(id)?;
        let bytes = self
            .with_store(|store| store.value(&record_key(id)))?
            .context("channel history not found")?;
        let record: ChannelRecord = serde_json::from_slice(&bytes)?;
        ensure!(
            record.terms.channel_id == id,
            "channel history identity mismatch"
        );
        record.terms.validate()?;
        Ok(record)
    }

    fn channel_config(&self) -> Result<ChainConfig> {
        self.ensure_mode(Mode::Paid)?;
        let config = self
            .config()
            .chain
            .context("local chain is not configured")?;
        config.validate()?;
        escrow(&config)?;
        let wallet = self.wallet.lock().expect("delivery wallet");
        ensure!(
            matches!(wallet.status(), WalletStatus::Unlocked { .. }),
            "unlock the local wallet for channel operations"
        );
        Ok(config)
    }

    pub(super) fn authorize_channel_spend(&self, transaction_id: &str) -> Result<()> {
        let (group, step) = transaction_id
            .split_once('/')
            .context("invalid channel transaction identity")?;
        intent(
            group
                .strip_prefix("channel-")
                .context("invalid channel owner intent")?,
        )?;
        let bytes = self
            .with_store(|store| store.value(&format!("local-spend:{group}")))?
            .context("channel spend has no local owner authorization")?;
        let operation: ChannelSpend = serde_json::from_slice(&bytes)?;
        ensure!(
            self.channel_config()? == operation.config
                && operation.owner_peer == self.identity.device_id()?
                && chain::address(&operation.wallet)?
                    == self.wallet.lock().expect("delivery wallet").address()?,
            "local channel spending authorization or deployment changed"
        );
        let (target, input) = operation
            .call(step)?
            .encode(&operation.config, chain::address(&operation.wallet)?)?;
        if let Some(transaction) =
            self.with_store(|store| store.find_transaction(transaction_id))?
        {
            ensure!(
                transaction.intent.wallet == operation.wallet
                    && transaction.intent.chain_id == operation.config.chain_id
                    && transaction.intent.to == chain::address_hex(target)
                    && transaction.intent.calldata == format!("0x{}", hex::encode(input))
                    && transaction.intent.gas_cap == operation.gas_cap,
                "channel transaction differs from its owner intent"
            );
        }
        Ok(())
    }

    async fn channel_spend(&self, intent_id: &str, operation: ChannelSpend) -> Result<Value> {
        intent(intent_id)?;
        ensure!(
            self.channel_config()? == operation.config,
            "channel deployment changed"
        );
        let group = format!("channel-{intent_id}");
        let key = format!("local-spend:{group}");
        self.with_store(|store| {
            if let Some(bytes) = store.value(&key)? {
                ensure!(
                    serde_json::from_slice::<ChannelSpend>(&bytes)? == operation,
                    "channel owner intent changed across retry"
                );
            } else {
                store.put_value(&key, &serde_json::to_vec(&operation)?)?;
            }
            Ok(())
        })?;
        let chain = chain::Chain::connect(operation.config.clone()).await?;
        if operation.approval {
            let observed = chain
                .execute(
                    self,
                    &format!("{group}/approve"),
                    chain::Call::Channel(operation.call("approve")?.into()),
                    &operation.gas_cap,
                )
                .await?;
            if observed.phase != "confirmed" {
                return Ok(json!({"step":"approve","transaction":observed}));
            }
        }
        let observed = chain
            .execute(
                self,
                &format!("{group}/execute"),
                chain::Call::Channel(operation.call("execute")?.into()),
                &operation.gas_cap,
            )
            .await?;
        Ok(json!({"step":"execute","transaction":observed}))
    }

    pub async fn local_channel(&self, command: ChannelCommand) -> Result<Value> {
        let _operation = self
            .operations
            .try_lock()
            .context("another delivery operation is in progress")?;
        match command {
            ChannelCommand::Export { channel_id } => {
                let voucher = self
                    .channel_record(&channel_id)?
                    .latest
                    .context("channel has no signed voucher")?;
                voucher.verify()?;
                Ok(serde_json::to_value(voucher)?)
            }
            ChannelCommand::Status { channel_id } => {
                let record = self.channel_record(&channel_id)?;
                let config = self
                    .config()
                    .chain
                    .context("local chain is not configured")?;
                record.terms.matches_config(&config)?;
                let chain = chain::Chain::connect(config).await?;
                let snapshot = chain.channel(word(&channel_id)?).await?;
                record.terms.matches_snapshot(&snapshot)?;
                Ok(
                    json!({"terms":record.terms,"authorized_amount":record.authorized_amount,"issued_sequence":record.issued_sequence,"issued_amount":record.issued_amount,
                    "latest_sequence":record.latest.as_ref().map(|v|v.sequence),"chain":snapshot}),
                )
            }
            ChannelCommand::Open {
                intent_id,
                payee,
                provider_peer,
                budget,
                gas_cap,
            } => {
                intent(&intent_id)?;
                let config = self.channel_config()?;
                let wallet = self.wallet.lock().expect("delivery wallet").address()?;
                let salt = keccak256(uuid::Uuid::parse_str(&intent_id)?.as_bytes());
                let channel_id = hex::encode(keccak256(
                    (config.chain_id, escrow(&config)?, wallet, salt).abi_encode_params(),
                ));
                let terms = ChannelTerms {
                    channel_id: channel_id.clone(),
                    chain_id: config.chain_id,
                    escrow: chain::address_hex(escrow(&config)?),
                    payer: chain::address_hex(wallet),
                    payee: chain::address_hex(chain::address(&payee)?),
                    provider_peer,
                    asset: chain::address_hex(chain::address(&config.asset)?),
                    budget: chain::amount(&budget)?.to_string(),
                    challenge_window: CHALLENGE_WINDOW,
                };
                terms.matches_config(&config)?;
                let group = format!("channel-{intent_id}");
                let existing =
                    self.with_store(|store| store.value(&format!("local-spend:{group}")))?;
                let approval = if let Some(bytes) = existing {
                    serde_json::from_slice::<ChannelSpend>(&bytes)?.approval
                } else {
                    chain::Chain::connect(config.clone())
                        .await?
                        .channel_allowance(wallet)
                        .await?
                        < chain::amount(&terms.budget)?
                };
                let record = ChannelRecord {
                    terms: terms.clone(),
                    authorized_amount: "0".into(),
                    issued_sequence: 0,
                    issued_amount: "0".into(),
                    latest: None,
                };
                let spend = ChannelSpend {
                    config,
                    owner_peer: self.identity.device_id()?,
                    wallet: terms.payer.clone(),
                    gas_cap,
                    action: ChannelAction::Open {
                        terms,
                        salt: hex::encode(salt),
                    },
                    approval,
                };
                self.with_store(|store| {
                    let key = record_key(&channel_id);
                    if let Some(bytes) = store.value(&key)? {
                        ensure!(
                            serde_json::from_slice::<ChannelRecord>(&bytes)?.terms == record.terms,
                            "channel opening terms changed"
                        );
                    } else {
                        store.put_value(&key, &serde_json::to_vec(&record)?)?;
                    }
                    Ok(())
                })?;
                let result = self.channel_spend(&intent_id, spend).await?;
                Ok(json!({"channel_id":channel_id,"result":result}))
            }
            ChannelCommand::Authorize {
                channel_id,
                order_id,
                amount,
                intent_id,
            } => {
                intent(&intent_id)?;
                word(&order_id)?;
                let config = self.channel_config()?;
                let mut record = self.channel_record(&channel_id)?;
                record.terms.matches_config(&config)?;
                ensure!(
                    chain::address(&record.terms.payer)?
                        == self.wallet.lock().expect("delivery wallet").address()?,
                    "only payer can authorize a Source fee"
                );
                let amount = chain::amount(&amount)?;
                ensure!(
                    amount > U256::ZERO && amount <= chain::amount(&record.terms.budget)?,
                    "fee must be positive whole token units within the budget"
                );
                let fee = FeeAuthorization {
                    channel_id,
                    order_id: order_id.clone(),
                    source_peer: self.identity.device_id()?,
                    manifest_hash: self.channel_source(
                        &order_id,
                        &record.terms.provider_peer,
                        false,
                    )?,
                    amount: amount.to_string(),
                    intent_id,
                };
                let snapshot = chain::Chain::connect(config)
                    .await?
                    .channel(word(&record.terms.channel_id)?)
                    .await?;
                record.terms.matches_snapshot(&snapshot)?;
                ensure!(
                    snapshot.state == ChannelState::Open,
                    "channel no longer accepts new fee authorizations"
                );
                self.with_store(|store| reserve_fee(store, &mut record, &fee))?;
                Ok(json!({"authorization":fee}))
            }
            ChannelCommand::Issue {
                order_id,
                intent_id,
            } => self.issue_channel(&order_id, &intent_id).await,
            ChannelCommand::Import { voucher } => {
                self.ensure_mode(Mode::Paid)?;
                voucher.verify()?;
                let config = self
                    .config()
                    .chain
                    .context("local chain is not configured")?;
                voucher.terms.matches_config(&config)?;
                ensure!(
                    voucher.terms.provider_peer == self.identity.device_id()?
                        && chain::address(&voucher.terms.payee)?
                            == self.wallet.lock().expect("delivery wallet").address()?,
                    "voucher Provider peer and fixed payee must both match this local Node"
                );
                let snapshot = chain::Chain::connect(config)
                    .await?
                    .channel(word(&voucher.terms.channel_id)?)
                    .await?;
                voucher.terms.matches_snapshot(&snapshot)?;
                self.with_store(|store| {
                    let key = record_key(&voucher.terms.channel_id);
                    let mut record = if let Some(bytes) = store.value(&key)? {
                        let record: ChannelRecord = serde_json::from_slice(&bytes)?;
                        ensure!(
                            record.terms == voucher.terms,
                            "imported channel terms changed"
                        );
                        record
                    } else {
                        ChannelRecord {
                            terms: voucher.terms.clone(),
                            authorized_amount: "0".into(),
                            issued_sequence: 0,
                            issued_amount: "0".into(),
                            latest: None,
                        }
                    };
                    if let Some(prior) = &record.latest {
                        if prior.sequence == voucher.sequence {
                            ensure!(
                                prior == &voucher,
                                "voucher sequence has conflicting contents"
                            );
                        } else {
                            ensure!(
                                prior.sequence < voucher.sequence
                                    && chain::amount(&prior.total_amount)?
                                        < chain::amount(&voucher.total_amount)?,
                                "imported voucher is stale or decreases the amount"
                            );
                        }
                    }
                    record.latest = Some(voucher.clone());
                    store.put_value(&key, &serde_json::to_vec(&record)?)
                })?;
                Ok(
                    json!({"channel_id":voucher.terms.channel_id,"sequence":voucher.sequence,"total_amount":voucher.total_amount}),
                )
            }
            ChannelCommand::Redeem {
                channel_id,
                intent_id,
                gas_cap,
            } => {
                intent(&intent_id)?;
                let existing = self
                    .with_store(|store| store.value(&format!("local-spend:channel-{intent_id}")))?;
                let voucher = if let Some(bytes) = existing {
                    let ChannelAction::Redeem { voucher } =
                        serde_json::from_slice::<ChannelSpend>(&bytes)?.action
                    else {
                        bail!("channel owner intent changed across retry");
                    };
                    ensure!(
                        voucher.terms.channel_id == channel_id,
                        "channel owner intent changed across retry"
                    );
                    voucher
                } else {
                    self.channel_record(&channel_id)?
                        .latest
                        .context("import a signed voucher before redemption")?
                };
                let config = self.channel_config()?;
                voucher.terms.matches_config(&config)?;
                let wallet =
                    chain::address_hex(self.wallet.lock().expect("delivery wallet").address()?);
                self.channel_spend(
                    &intent_id,
                    ChannelSpend {
                        config,
                        owner_peer: self.identity.device_id()?,
                        wallet,
                        gas_cap,
                        action: ChannelAction::Redeem { voucher },
                        approval: false,
                    },
                )
                .await
            }
            ChannelCommand::Close {
                channel_id,
                intent_id,
                gas_cap,
            } => {
                self.channel_exit(channel_id, intent_id, gas_cap, false)
                    .await
            }
            ChannelCommand::Refund {
                channel_id,
                intent_id,
                gas_cap,
            } => {
                self.channel_exit(channel_id, intent_id, gas_cap, true)
                    .await
            }
        }
    }

    async fn channel_exit(
        &self,
        channel_id: String,
        intent_id: String,
        gas_cap: String,
        refund: bool,
    ) -> Result<Value> {
        let config = self.channel_config()?;
        let record = self.channel_record(&channel_id)?;
        record.terms.matches_config(&config)?;
        let wallet = self.wallet.lock().expect("delivery wallet").address()?;
        ensure!(
            refund || chain::address(&record.terms.payer)? == wallet,
            "only the fixed payer may start closing"
        );
        self.channel_spend(
            &intent_id,
            ChannelSpend {
                config,
                owner_peer: self.identity.device_id()?,
                wallet: chain::address_hex(wallet),
                gas_cap,
                action: if refund {
                    ChannelAction::Refund { channel_id }
                } else {
                    ChannelAction::Close { channel_id }
                },
                approval: false,
            },
        )
        .await
    }

    async fn issue_channel(&self, order_id: &str, intent_id: &str) -> Result<Value> {
        intent(intent_id)?;
        word(order_id)?;
        let config = self.channel_config()?;
        let fee: FeeAuthorization = serde_json::from_slice(
            &self
                .with_store(|store| store.value(&fee_key(order_id)))?
                .context("authorize this Source order's exact fee locally first")?,
        )?;
        ensure!(
            fee.order_id == order_id
                && fee.intent_id == intent_id
                && fee.source_peer == self.identity.device_id()?,
            "Source charge differs from immutable owner authorization"
        );
        let mut record = self.channel_record(&fee.channel_id)?;
        record.terms.matches_config(&config)?;
        ensure!(
            fee.manifest_hash
                == self.channel_source(order_id, &record.terms.provider_peer, false)?,
            "authorized Source manifest changed"
        );
        let receipt_commitment =
            self.channel_source(order_id, &record.terms.provider_peer, true)?;
        if let Some(bytes) = self.with_store(|store| store.value(&charge_key(order_id)))? {
            let voucher: CumulativeVoucher = serde_json::from_slice(&bytes)?;
            ensure!(
                voucher.terms == record.terms && voucher.receipt_commitment == receipt_commitment,
                "Source receipt already charged to another channel"
            );
            voucher.verify()?;
            return Ok(serde_json::to_value(voucher)?);
        }
        let chain = chain::Chain::connect(config.clone()).await?;
        let snapshot = chain.channel(word(&fee.channel_id)?).await?;
        record.terms.matches_snapshot(&snapshot)?;
        ensure!(
            snapshot.state == ChannelState::Open
                && snapshot.sequence <= record.issued_sequence
                && chain::amount(&snapshot.redeemed)? <= chain::amount(&record.issued_amount)?,
            "channel is closing or issuer accounting is behind the chain"
        );
        let total = chain::amount(&record.issued_amount)?
            .checked_add(chain::amount(&fee.amount)?)
            .context("cumulative fee overflow")?;
        ensure!(
            total <= chain::amount(&record.terms.budget)?
                && total <= chain::amount(&record.authorized_amount)?,
            "cumulative Source fees exceed the channel deposit"
        );
        let sequence = record
            .issued_sequence
            .checked_add(1)
            .context("channel sequence exhausted")?;
        let mut voucher = CumulativeVoucher {
            terms: record.terms.clone(),
            sequence,
            total_amount: total.to_string(),
            receipt_commitment,
            signature: String::new(),
        };
        // The settings lock keeps config changes from crossing the sign-and-persist boundary.
        let store = self.store.lock().expect("delivery store");
        let settings = self.settings.lock().expect("delivery settings");
        ensure!(
            settings.enabled && settings.paid_enabled && settings.chain.as_ref() == Some(&config),
            "channel authorization changed before signing"
        );
        voucher.signature = hex::encode(
            self.wallet
                .lock()
                .expect("delivery wallet")
                .sign_channel_voucher(&voucher)?,
        );
        voucher.verify()?;
        record.issued_sequence = sequence;
        record.issued_amount = total.to_string();
        record.latest = Some(voucher.clone());
        let encoded_record = serde_json::to_vec(&record)?;
        let encoded_voucher = serde_json::to_vec(&voucher)?;
        // Avoid with_store while holding settings, since it reads the current storage limits.
        store
            .as_ref()
            .context("channel store unavailable")?
            .put_values(&[
                (&record_key(&fee.channel_id), &encoded_record),
                (&charge_key(order_id), &encoded_voucher),
            ])?;
        Ok(serde_json::to_value(voucher)?)
    }
}
