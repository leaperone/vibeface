use super::{
    Delivery,
    chain::{self, ChainConfig, ChainPosition, abi},
    types::{Mode, Role, StoredOrder, parse_word},
    wallet::WalletStatus,
};
use alloy_primitives::{Address, B256, Bytes, U256};
use alloy_sol_types::{SolCall, SolValue};
use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use vibeface_delivery::{Order, Relation, Statement, Word, hash};

pub const MAX_BATCH_WINDOWS: usize = 2;
pub(super) const REVEAL_GAS_LIMIT: u64 = 800_000;
pub(super) const PUBLICATION_STEP_SECONDS: u64 = 10;
const MAX_ACTIVE_CLAIMS: usize = 2;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PublicationPolicy {
    pub watchdog_seconds: u64,
    pub inclusion_seconds: u64,
    pub finality_seconds: u64,
    pub clock_skew_seconds: u64,
    pub max_fee_per_gas: String,
    pub priority_fee_per_gas: String,
}
impl PublicationPolicy {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.watchdog_seconds >= (MAX_ACTIVE_CLAIMS as u64 + 1) * PUBLICATION_STEP_SECONDS + 2
                && self.inclusion_seconds > 0
                && self.finality_seconds > 0
                && self.clock_skew_seconds > 0,
            "publication policy must cover the bounded two-claim watchdog and explicit chain/clock assumptions"
        );
        self.margin()?;
        let fee = chain::amount(&self.max_fee_per_gas)?;
        ensure!(
            fee > U256::ZERO
                && fee <= U256::from(u128::MAX)
                && chain::amount(&self.priority_fee_per_gas)? <= fee,
            "invalid fixed local publication fee policy"
        );
        Ok(())
    }
    fn margin(&self) -> Result<u64> {
        self.watchdog_seconds
            .checked_add(self.inclusion_seconds)
            .and_then(|n| n.checked_add(self.finality_seconds))
            .and_then(|n| n.checked_add(self.clock_skew_seconds))
            .context("publication safety margin overflow")
    }
    fn first_allowed(&self, now: u64, cutoff: u64) -> Result<bool> {
        Ok(now
            .checked_add(self.margin()?)
            .is_some_and(|latest| latest < cutoff))
    }
    fn disclosure_allowed(&self, state: &ClaimState, now: u64, cutoff: u64) -> Result<bool> {
        if state.first_possible_at().is_some() {
            Ok(now < cutoff)
        } else {
            self.first_allowed(now, cutoff)
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BudgetTerms {
    pub budget_id: String,
    pub chain_id: u64,
    pub escrow: String,
    pub payer: String,
    pub payee: String,
    pub provider_peer: String,
    pub asset: String,
    pub deposit: String,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BudgetSnapshot {
    pub terms: BudgetTerms,
    pub locked: String,
    pub earned: String,
    pub withdrawn: String,
    pub refunded: String,
    pub free: String,
    pub max_claim_until: u64,
    pub state: String,
    pub position: ChainPosition,
}
impl BudgetSnapshot {
    pub(super) fn from_chain(
        config: &ChainConfig,
        id: B256,
        b: abi::ChainBudget,
        position: ChainPosition,
    ) -> Result<Self> {
        ensure!(
            b.payer != Address::ZERO
                && b.payee != Address::ZERO
                && b.payer != b.payee
                && b.providerPeer != B256::ZERO
                && b.asset == chain::address(&config.asset)?
                && b.assetDecimals == 6
                && (1..=3).contains(&b.state),
            "invalid protected budget deployment or terms"
        );
        let free = b
            .deposit
            .checked_sub(b.locked)
            .and_then(|n| n.checked_sub(b.earned))
            .and_then(|n| n.checked_sub(b.refunded))
            .context("protected budget conservation failed")?;
        ensure!(
            b.withdrawn <= b.earned,
            "protected budget withdrawal exceeds earned"
        );
        Ok(Self {
            terms: BudgetTerms {
                budget_id: hex::encode(id),
                chain_id: config.chain_id,
                escrow: config.escrow.clone(),
                payer: chain::address_hex(b.payer),
                payee: chain::address_hex(b.payee),
                provider_peer: hex::encode(b.providerPeer),
                asset: chain::address_hex(b.asset),
                deposit: b.deposit.to_string(),
            },
            locked: b.locked.to_string(),
            earned: b.earned.to_string(),
            withdrawn: b.withdrawn.to_string(),
            refunded: b.refunded.to_string(),
            free: free.to_string(),
            max_claim_until: b.maxClaimUntil,
            state: match b.state {
                1 => "open",
                2 => "closing",
                _ => "closed",
            }
            .into(),
            position,
        })
    }
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct BudgetRecord {
    pub terms: BudgetTerms,
    pub payer_gas_cap: String,
    pub provider_gas_cap: String,
    pub network_id: Option<String>,
    pub authorized_by: Option<String>,
}
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WindowRef {
    pub order_id: String,
    pub index: u16,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HoldAuthorization {
    pub window: WindowRef,
    pub seal: Vec<u8>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Reveal {
    pub window: WindowRef,
    pub capsule: Vec<u8>,
}
#[derive(Clone)]
pub struct Reservation {
    pub order: Order,
    pub source_seals: Vec<Vec<u8>>,
    pub wallet_signature: Vec<u8>,
}

pub enum BudgetAction {
    Open {
        salt: Word,
        payee: Address,
        provider_peer: Word,
        deposit: String,
    },
    Reserve {
        budget_id: Word,
        reservations: Vec<Reservation>,
    },
    Hold {
        budget_id: Word,
        items: Vec<HoldAuthorization>,
    },
    Reveal {
        budget_id: Word,
        items: Vec<Reveal>,
    },
    Withdraw {
        budget_id: Word,
    },
    Close {
        budget_id: Word,
    },
    Refund {
        budget_id: Word,
        windows: Vec<WindowRef>,
    },
}
fn count(count: usize) -> Result<()> {
    ensure!(
        (1..=MAX_BATCH_WINDOWS).contains(&count),
        "a protected batch contains at most two windows"
    );
    Ok(())
}
fn window(window: &WindowRef) -> Result<abi::WindowRef> {
    chain::validate_order_index(parse_word(&window.order_id)?, window.index)?;
    ensure!(window.index < 2, "protected window exceeds Node profile");
    Ok(abi::WindowRef {
        orderId: parse_word(&window.order_id)?.into(),
        index: window.index,
    })
}
fn source(config: &ChainConfig, r: Reservation, wallet: Address) -> Result<abi::Reservation> {
    r.order.verify()?;
    config.profile.check_order(&r.order.unsigned)?;
    ensure!(
        r.order.unsigned.terms.payer == wallet.into_array()
            && r.order.unsigned.terms.asset == chain::address(&config.asset)?.into_array()
            && r.order.unsigned.terms.acceptance == vibeface_delivery::Acceptance::Recoverable
            && r.source_seals.len() == r.order.unsigned.windows.len()
            && r.wallet_signature.len() == 65,
        "protected reservation has invalid payer, asset, acceptance or proofs"
    );
    for seal in &r.source_seals {
        chain::validate_seal(seal)?;
    }
    let input = abi::DeliveryStatement::abi_decode_validate(
        Statement::window(&r.order.unsigned, 0, Relation::Source)?.abi_bytes(),
    )?;
    chain::validate_statement(config, &input)?;
    Ok(abi::Reservation {
        input,
        specs: r
            .order
            .unsigned
            .windows
            .iter()
            .map(|s| abi::WindowSpec {
                index: s.index,
                offset: s.offset,
                plaintextBytes: s.plaintext_bytes,
                ciphertextBytes: s.ciphertext_bytes,
                ciphertextHash: s.ciphertext_hash.into(),
                capsuleHash: s.capsule_hash.into(),
                plaintextCommitment: s.plaintext_commitment.into(),
                deliveryFee: U256::from_be_bytes(s.delivery_fee.to_be_bytes()),
                reservationFee: U256::from_be_bytes(s.reservation_fee.to_be_bytes()),
            })
            .collect(),
        seals: r.source_seals.into_iter().map(Bytes::from).collect(),
        walletSignature: r.wallet_signature.into(),
    })
}
impl BudgetAction {
    pub(super) fn encode(
        self,
        config: &ChainConfig,
        wallet: Address,
    ) -> Result<(Address, Vec<u8>)> {
        let input = match self {
            Self::Open {
                salt,
                payee,
                provider_peer,
                deposit,
            } => abi::openBudgetCall {
                salt: salt.into(),
                payee,
                providerPeer: provider_peer.into(),
                asset: chain::address(&config.asset)?,
                deposit: chain::amount(&deposit)?,
            }
            .abi_encode(),
            Self::Reserve {
                budget_id,
                reservations,
            } => {
                count(
                    reservations
                        .iter()
                        .map(|r| r.order.unsigned.windows.len())
                        .sum(),
                )?;
                abi::reserveBatchCall {
                    budgetId: budget_id.into(),
                    reservations: reservations
                        .into_iter()
                        .map(|r| source(config, r, wallet))
                        .collect::<Result<_>>()?,
                }
                .abi_encode()
            }
            Self::Hold { budget_id, items } => {
                count(items.len())?;
                abi::holdBatchCall {
                    budgetId: budget_id.into(),
                    items: items
                        .into_iter()
                        .map(|item| {
                            let w = window(&item.window)?;
                            chain::validate_seal(&item.seal)?;
                            Ok(abi::HoldItem {
                                orderId: w.orderId,
                                index: w.index,
                                seal: item.seal.into(),
                            })
                        })
                        .collect::<Result<_>>()?,
                }
                .abi_encode()
            }
            Self::Reveal { budget_id, items } => {
                count(items.len())?;
                abi::revealBatchCall {
                    budgetId: budget_id.into(),
                    items: items
                        .into_iter()
                        .map(|item| {
                            let w = window(&item.window)?;
                            ensure!(
                                item.capsule.len() == 80,
                                "protected capsule must contain 80 bytes"
                            );
                            Ok(abi::RevealItem {
                                orderId: w.orderId,
                                index: w.index,
                                capsule: item.capsule.into(),
                            })
                        })
                        .collect::<Result<_>>()?,
                }
                .abi_encode()
            }
            Self::Withdraw { budget_id } => abi::withdrawBudgetCall {
                id: budget_id.into(),
            }
            .abi_encode(),
            Self::Close { budget_id } => abi::closeBudgetCall {
                id: budget_id.into(),
            }
            .abi_encode(),
            Self::Refund { budget_id, windows } => {
                ensure!(
                    windows.len() <= MAX_BATCH_WINDOWS,
                    "refund batch exceeds two windows"
                );
                abi::refundBudgetCall {
                    id: budget_id.into(),
                    items: windows.iter().map(window).collect::<Result<_>>()?,
                }
                .abi_encode()
            }
        };
        validate_calldata(config, &input, wallet)?;
        Ok((chain::address(&config.escrow)?, input))
    }
}
pub(super) fn validate_calldata(config: &ChainConfig, input: &[u8], wallet: Address) -> Result<()> {
    let selector = input
        .get(..4)
        .context("missing protected budget selector")?;
    macro_rules! exact {
        ($name:ident) => {{
            let call = abi::$name::abi_decode_validate(input)?;
            ensure!(
                call.abi_encode() == input,
                "noncanonical protected budget calldata"
            );
            call
        }};
    }
    let id = match selector {
        s if s == abi::openBudgetCall::SELECTOR => {
            let c = exact!(openBudgetCall);
            ensure!(
                c.salt != B256::ZERO
                    && c.providerPeer != B256::ZERO
                    && c.payee != Address::ZERO
                    && c.payee != wallet
                    && c.payee != chain::address(&config.escrow)?
                    && c.asset == chain::address(&config.asset)?
                    && c.deposit > U256::ZERO,
                "invalid fixed protected budget terms"
            );
            return Ok(());
        }
        s if s == abi::reserveBatchCall::SELECTOR => {
            let c = exact!(reserveBatchCall);
            count(c.reservations.iter().map(|r| r.specs.len()).sum())?;
            for r in c.reservations {
                chain::validate_statement(config, &r.input)?;
                ensure!(
                    r.input.relation == 2
                        && r.input.acceptance == 0
                        && r.input.payer == wallet
                        && r.input.asset == chain::address(&config.asset)?
                        && !r.specs.is_empty()
                        && r.specs.len() == r.input.windowCount as usize
                        && r.seals.len() == r.specs.len()
                        && r.walletSignature.len() == 65,
                    "invalid authorized protected reservation"
                );
                for seal in r.seals {
                    chain::validate_seal(&seal)?;
                }
            }
            c.budgetId
        }
        s if s == abi::holdBatchCall::SELECTOR => {
            let c = exact!(holdBatchCall);
            count(c.items.len())?;
            for item in c.items {
                window(&WindowRef {
                    order_id: hex::encode(item.orderId),
                    index: item.index,
                })?;
                chain::validate_seal(&item.seal)?;
            }
            c.budgetId
        }
        s if s == abi::revealBatchCall::SELECTOR => {
            let c = exact!(revealBatchCall);
            count(c.items.len())?;
            let mut seen = std::collections::BTreeSet::new();
            for item in c.items {
                let w = WindowRef {
                    order_id: hex::encode(item.orderId),
                    index: item.index,
                };
                window(&w)?;
                ensure!(
                    item.capsule.len() == 80 && seen.insert(w),
                    "invalid or duplicate protected reveal window"
                );
            }
            c.budgetId
        }
        s if s == abi::withdrawBudgetCall::SELECTOR => exact!(withdrawBudgetCall).id,
        s if s == abi::closeBudgetCall::SELECTOR => exact!(closeBudgetCall).id,
        s if s == abi::refundBudgetCall::SELECTOR => {
            let c = exact!(refundBudgetCall);
            ensure!(
                c.items.len() <= MAX_BATCH_WINDOWS,
                "refund batch exceeds two windows"
            );
            for item in c.items {
                window(&WindowRef {
                    order_id: hex::encode(item.orderId),
                    index: item.index,
                })?;
            }
            c.id
        }
        _ => bail!("delivery wallet refuses an unknown or unprotected contract action"),
    };
    ensure!(id != B256::ZERO, "protected budget id is zero");
    Ok(())
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub enum BudgetCommand {
    Open {
        payee: String,
        provider_peer: String,
        deposit: String,
        intent_id: String,
        gas_cap: String,
    },
    Reserve {
        budget_id: String,
        order_ids: Vec<String>,
        intent_id: String,
    },
    Flush {
        budget_id: String,
    },
    Withdraw {
        budget_id: String,
        intent_id: String,
    },
    Close {
        budget_id: String,
        intent_id: String,
    },
    Refund {
        budget_id: String,
        windows: Vec<WindowRef>,
        intent_id: String,
    },
    Status {
        budget_id: Option<String>,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ClaimTerms {
    budget_id: String,
    payee: String,
    items: Vec<Reveal>,
    economic_cutoff: u64,
    total_amount: String,
    gas_cap: String,
    policy: PublicationPolicy,
    expected_earned: std::collections::BTreeMap<String, String>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
enum ClaimState {
    PrivateReady,
    DisclosedOutstanding { first_possible_at: u64 },
    FinalEarned { first_possible_at: Option<u64> },
    ExpiredUnrevealed,
}
impl ClaimState {
    fn first_possible_at(&self) -> Option<u64> {
        match self {
            Self::DisclosedOutstanding { first_possible_at } => Some(*first_possible_at),
            Self::FinalEarned { first_possible_at } => *first_possible_at,
            _ => None,
        }
    }
    fn name(&self) -> &'static str {
        match self {
            Self::PrivateReady => "private_ready",
            Self::DisclosedOutstanding { .. } => "disclosed_outstanding",
            Self::FinalEarned { .. } => "final_earned",
            Self::ExpiredUnrevealed => "expired_unrevealed",
        }
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PublicationClaim {
    id: String,
    terms: ClaimTerms,
    state: ClaimState,
    attempts: Vec<String>,
    risk: Option<String>,
    checked_at_ms: i64,
}
impl PublicationClaim {
    fn key(&self) -> String {
        format!("protected-claim:{}", self.id)
    }
    fn summary(&self) -> Value {
        let windows: Vec<_> = self.terms.items.iter().map(|item| &item.window).collect();
        json!({"claim_id":self.id,"budget_id":self.terms.budget_id,"payee":self.terms.payee,
        "state":self.state.name(),"economic_cutoff":self.terms.economic_cutoff,"total_amount":self.terms.total_amount,
        "gas_cap":self.terms.gas_cap,"attempt_ids":self.attempts,"windows":windows,"first_possible_at":self.state.first_possible_at(),"risk":self.risk})
    }
    fn occupies_publication_window(&self, now: u64) -> Result<bool> {
        Ok(match self.state {
            ClaimState::PrivateReady | ClaimState::DisclosedOutstanding { .. } => true,
            ClaimState::FinalEarned {
                first_possible_at: Some(at),
            } => at
                .checked_add(self.terms.policy.margin()?)
                .is_none_or(|until| now < until),
            _ => false,
        })
    }
}
fn record_key(id: &str) -> String {
    format!("protected-budget:{id}")
}
fn hold_key(w: &WindowRef) -> String {
    format!("protected-hold:{}:{}", w.order_id, w.index)
}
fn claim_window_key(w: &WindowRef) -> String {
    format!("protected-claim-window:{}:{}", w.order_id, w.index)
}
fn group(id: &str) -> String {
    format!("budget-{id}")
}
fn uuid(value: &str) -> Result<()> {
    ensure!(
        uuid::Uuid::parse_str(value)?.to_string() == value,
        "intent_id must be a canonical UUID"
    );
    Ok(())
}

impl Delivery {
    pub(super) fn budget_record(&self, id: &str) -> Result<BudgetRecord> {
        parse_word(id)?;
        serde_json::from_slice(
            &self
                .with_store(|s| s.value(&record_key(id)))?
                .context("budget is not registered on this Node")?,
        )
        .map_err(Into::into)
    }
    pub(super) fn register_budget(
        &self,
        snapshot: &BudgetSnapshot,
        payer_cap: &str,
    ) -> Result<BudgetRecord> {
        if let Some(body) = self.with_store(|s| s.value(&record_key(&snapshot.terms.budget_id)))? {
            let prior: BudgetRecord = serde_json::from_slice(&body)?;
            ensure!(
                prior.terms == snapshot.terms,
                "protected budget terms changed"
            );
            return Ok(prior);
        }
        chain::amount(payer_cap)?;
        let record = BudgetRecord {
            terms: snapshot.terms.clone(),
            payer_gas_cap: payer_cap.into(),
            provider_gas_cap: self.config().provider_gas_cap,
            network_id: None,
            authorized_by: None,
        };
        self.with_store(|s| {
            s.put_value(
                &record_key(&record.terms.budget_id),
                &serde_json::to_vec(&record)?,
            )
        })?;
        Ok(record)
    }
    pub(super) async fn budget_snapshot(&self, id: &str) -> Result<BudgetSnapshot> {
        let chain = self.chain().await?;
        let position = chain.confirmed_position().await?;
        let result = chain.budget_at(&position, parse_word(id)?.into()).await?;
        chain.verify_position(&position).await?;
        Ok(result)
    }
    pub(super) async fn authorize_budget_spend(&self, id: &str, chain: &ChainConfig) -> Result<()> {
        let (prefix, _) = id
            .split_once('/')
            .context("invalid budget transaction id")?;
        let record = self.budget_record(
            prefix
                .strip_prefix("budget-")
                .context("invalid budget spend group")?,
        )?;
        ensure!(
            record.terms.chain_id == chain.chain_id
                && chain::address(&record.terms.escrow)? == chain::address(&chain.escrow)?
                && chain::address(&record.terms.asset)? == chain::address(&chain.asset)?,
            "budget belongs to another delivery deployment; preserve its original transaction"
        );
        let wallet = self
            .wallet
            .lock()
            .map_err(|_| anyhow::anyhow!("delivery wallet lock failed"))?
            .address()?;
        let owner_intent = self
            .with_store(|store| store.value(&format!("local-spend:{id}")))?
            .is_some();
        if chain::address_hex(wallet) == record.terms.payer {
            if !owner_intent {
                self.authorize_effect(
                    record
                        .network_id
                        .as_deref()
                        .context("payer budget needs its managing network authorization")?,
                    record
                        .authorized_by
                        .as_deref()
                        .context("payer budget needs its managing member authorization")?,
                )
                .await?;
            }
        } else {
            ensure!(
                chain::address_hex(wallet) == record.terms.payee
                    && self.identity.device_id()? == record.terms.provider_peer,
                "protected budget belongs to another wallet or provider Node"
            );
            ensure!(
                owner_intent || self.config().provider_gas_cap == record.provider_gas_cap,
                "provider gas authorization changed; automatic budget spending is paused"
            );
        }
        Ok(())
    }
    fn claims(&self) -> Result<Vec<PublicationClaim>> {
        self.with_store(|s| s.values_with_prefix("protected-claim:"))?
            .into_iter()
            .map(|body| serde_json::from_slice(&body).map_err(Into::into))
            .collect()
    }
    fn save_claim(&self, claim: &PublicationClaim) -> Result<()> {
        self.with_store(|s| s.put_value(&claim.key(), &serde_json::to_vec(claim)?))
    }
    pub(super) fn publication_status(
        &self,
        order_id: &str,
    ) -> Result<Option<super::types::PublicationView>> {
        Ok(self
            .claims()?
            .into_iter()
            .find(|c| {
                c.terms
                    .items
                    .iter()
                    .any(|item| item.window.order_id == order_id)
            })
            .map(|claim| super::types::PublicationView {
                state: claim.state.name().into(),
                total_amount: claim.terms.total_amount,
                economic_cutoff: claim.terms.economic_cutoff,
                availability_risk: claim.risk,
            }))
    }
    pub(super) fn queue_hold(&self, order: &StoredOrder, index: u16, seal: Vec<u8>) -> Result<()> {
        let item = HoldAuthorization {
            window: WindowRef {
                order_id: order.view.order_id.clone(),
                index,
            },
            seal,
        };
        let key = hold_key(&item.window);
        let body = serde_json::to_vec(&item)?;
        self.with_store(|s| {
            if let Some(prior) = s.value(&key)? {
                ensure!(prior == body, "hold authorization changed across retry");
            }
            s.put_value(&key, &body)
        })?;
        self.wakeup.notify_one();
        Ok(())
    }
    pub async fn local_budget(&self, command: BudgetCommand) -> Result<Value> {
        if let BudgetCommand::Status { budget_id } = command {
            if !self.data_dir.join("delivery/service.sqlite3").exists() && budget_id.is_none() {
                return Ok(json!({"budgets":[],"claims":[],"execution_enabled":false}));
            }
            self.run_publications(false).await?;
            let ids = if let Some(id) = budget_id {
                vec![id]
            } else {
                self.with_store(|s| s.values_with_prefix("protected-budget:"))?
                    .into_iter()
                    .map(|body| {
                        Ok(serde_json::from_slice::<BudgetRecord>(&body)?
                            .terms
                            .budget_id)
                    })
                    .collect::<Result<Vec<_>>>()?
            };
            let mut snapshots = Vec::new();
            let mut ready_windows = Vec::new();
            for id in &ids {
                snapshots.push(self.budget_snapshot(id).await?);
                for window in self.budget_ready_windows(id).await? {
                    ready_windows.push(
                        json!({"budget_id":id,"order_id":window.order_id,"index":window.index}),
                    );
                }
            }
            let claims: Vec<_> = self
                .claims()?
                .into_iter()
                .filter(|c| ids.contains(&c.terms.budget_id))
                .map(|c| c.summary())
                .collect();
            return Ok(
                json!({"budgets":snapshots,"claims":claims,"ready_windows":ready_windows,"execution_enabled":self.config().enabled && self.config().paid_enabled,
                "availability":"locking the wallet or disabling paid delivery pauses publication recovery; read-only reconciliation remains available"}),
            );
        }
        self.ensure_mode(Mode::Paid)?;
        match command {
            BudgetCommand::Open {
                payee,
                provider_peer,
                deposit,
                intent_id,
                gas_cap,
            } => {
                uuid(&intent_id)?;
                let chain = self.chain().await?;
                let payer = self
                    .wallet
                    .lock()
                    .map_err(|_| anyhow::anyhow!("delivery wallet lock failed"))?
                    .address()?;
                let config = chain.config();
                let salt = hash(intent_id.as_bytes());
                let encoded = (
                    B256::from(hash(b"vibeface-delivery-budget-v1")),
                    U256::from(config.chain_id),
                    chain::address(&config.escrow)?,
                    payer,
                    B256::from(salt),
                )
                    .abi_encode();
                let budget_id = hex::encode(hash(&encoded));
                let terms = BudgetTerms {
                    budget_id: budget_id.clone(),
                    chain_id: config.chain_id,
                    escrow: config.escrow.clone(),
                    payer: chain::address_hex(payer),
                    payee: chain::address_hex(chain::address(&payee)?),
                    provider_peer: hex::encode(parse_word(&provider_peer)?),
                    asset: config.asset.clone(),
                    deposit: chain::amount(&deposit)?.to_string(),
                };
                let record = BudgetRecord {
                    terms,
                    payer_gas_cap: gas_cap.clone(),
                    provider_gas_cap: self.config().provider_gas_cap,
                    network_id: None,
                    authorized_by: None,
                };
                let key = record_key(&budget_id);
                self.with_store(|s| {
                    if let Some(prior) = s.value(&key)? {
                        let prior: BudgetRecord = serde_json::from_slice(&prior)?;
                        ensure!(
                            prior.terms == record.terms
                                && prior.payer_gas_cap == record.payer_gas_cap,
                            "budget open intent or cumulative gas authorization changed"
                        );
                        return Ok(());
                    }
                    s.put_value(&key, &serde_json::to_vec(&record)?)
                })?;
                for action in ["approve", "open"] {
                    self.with_store(|s| {
                        s.put_value(
                            &format!("local-spend:{}/{action}", group(&budget_id)),
                            b"owner",
                        )
                    })?;
                }
                let approve = chain
                    .execute(
                        self,
                        &format!("{}/approve", group(&budget_id)),
                        chain::Call::Approve {
                            amount: vibeface_delivery::Amount::from_decimal_str(&deposit)?,
                        },
                        &gas_cap,
                    )
                    .await?;
                if approve.phase != "confirmed" {
                    return Ok(json!({"budget_id":budget_id,"transaction":approve}));
                }
                let result = chain
                    .execute(
                        self,
                        &format!("{}/open", group(&budget_id)),
                        chain::Call::Budget(Box::new(BudgetAction::Open {
                            salt,
                            payee: chain::address(&payee)?,
                            provider_peer: parse_word(&provider_peer)?,
                            deposit,
                        })),
                        &gas_cap,
                    )
                    .await?;
                Ok(json!({"budget_id":budget_id,"transaction":result}))
            }
            BudgetCommand::Reserve {
                budget_id,
                order_ids,
                intent_id,
            } => {
                uuid(&intent_id)?;
                let mut orders = Vec::new();
                for id in order_ids {
                    orders.push(self.with_store(|s| s.order(&id))?);
                }
                Ok(serde_json::to_value(
                    self.reserve_orders(&budget_id, &orders, &intent_id).await?,
                )?)
            }
            BudgetCommand::Flush { budget_id } => {
                let _provider = self.provider_operations.lock().await;
                self.authorize_budget_batch(&budget_id).await?;
                self.flush_budget_holds(&budget_id).await?;
                self.run_publications(true).await?;
                Ok(json!({"budget_id":budget_id,"queued":true}))
            }
            BudgetCommand::Withdraw {
                budget_id,
                intent_id,
            } => {
                uuid(&intent_id)?;
                self.budget_transaction(
                    &budget_id,
                    &intent_id,
                    BudgetAction::Withdraw {
                        budget_id: parse_word(&budget_id)?,
                    },
                )
                .await
            }
            BudgetCommand::Close {
                budget_id,
                intent_id,
            } => {
                uuid(&intent_id)?;
                self.budget_transaction(
                    &budget_id,
                    &intent_id,
                    BudgetAction::Close {
                        budget_id: parse_word(&budget_id)?,
                    },
                )
                .await
            }
            BudgetCommand::Refund {
                budget_id,
                windows,
                intent_id,
            } => {
                uuid(&intent_id)?;
                self.budget_transaction(
                    &budget_id,
                    &intent_id,
                    BudgetAction::Refund {
                        budget_id: parse_word(&budget_id)?,
                        windows,
                    },
                )
                .await
            }
            BudgetCommand::Status { .. } => unreachable!(),
        }
    }
    pub(super) async fn client_budgets(&self, network: &str) -> Result<Value> {
        if !self.data_dir.join("delivery/service.sqlite3").exists() {
            return Ok(json!({"budgets":[]}));
        }
        let wallet = self
            .wallet
            .lock()
            .map_err(|_| anyhow::anyhow!("delivery wallet lock failed"))?
            .address()
            .ok()
            .map(chain::address_hex);
        let records: Vec<BudgetRecord> = self
            .with_store(|s| s.values_with_prefix("protected-budget:"))?
            .into_iter()
            .map(|body| serde_json::from_slice(&body).map_err(Into::into))
            .collect::<Result<_>>()?;
        let mut budgets = Vec::new();
        for record in records {
            if record.network_id.as_deref().is_some_and(|id| id != network)
                || wallet.as_deref().is_none_or(|wallet| {
                    wallet != record.terms.payer && wallet != record.terms.payee
                })
            {
                continue;
            }
            let b = self.budget_snapshot(&record.terms.budget_id).await?;
            let ready_windows = self.budget_ready_windows(&record.terms.budget_id).await?;
            budgets.push(json!({"budget_id":b.terms.budget_id,"payer":b.terms.payer,"payee":b.terms.payee,"provider_peer":b.terms.provider_peer,
                "asset":b.terms.asset,"deposit":b.terms.deposit,"free":b.free,"locked":b.locked,"earned":b.earned,"withdrawn":b.withdrawn,
                "refunded":b.refunded,"state":b.state,"gas_cap":record.payer_gas_cap,"ready_windows":ready_windows}));
        }
        Ok(json!({"budgets":budgets}))
    }
    async fn budget_ready_windows(&self, id: &str) -> Result<Vec<WindowRef>> {
        let orders = self.with_store(|s| s.orders(None))?;
        let mut ready = Vec::new();
        for order in orders.into_iter().filter(|o| {
            o.accepted
                && o.view.role == Role::Provider
                && o.view.payment.budget_id.as_deref() == Some(id)
        }) {
            let state = self.paid_state(&order.order).await?;
            for (index, fact) in state.windows.iter().enumerate() {
                let window = WindowRef {
                    order_id: order.view.order_id.clone(),
                    index: index as u16,
                };
                if matches!(fact.phase, 0 | 1)
                    && self.with_store(|s| s.value(&hold_key(&window)))?.is_some()
                    && self
                        .with_store(|s| s.value(&claim_window_key(&window)))?
                        .is_none()
                    && self.with_store(|s| {
                        s.object_complete(&window.order_id, window.index, "capsule")
                    })?
                {
                    ready.push(window);
                }
            }
        }
        Ok(ready)
    }
    async fn budget_transaction(
        &self,
        id: &str,
        intent: &str,
        action: BudgetAction,
    ) -> Result<Value> {
        let record = self.budget_record(id)?;
        let wallet = self
            .wallet
            .lock()
            .map_err(|_| anyhow::anyhow!("delivery wallet lock failed"))?
            .address()?;
        let cap = if chain::address_hex(wallet) == record.terms.payer {
            &record.payer_gas_cap
        } else {
            &record.provider_gas_cap
        };
        let transaction_id = format!("{}/{intent}", group(id));
        self.with_store(|store| {
            store.put_value(&format!("local-spend:{transaction_id}"), b"owner")
        })?;
        Ok(serde_json::to_value(
            self.chain()
                .await?
                .execute(
                    self,
                    &transaction_id,
                    chain::Call::Budget(Box::new(action)),
                    cap,
                )
                .await?,
        )?)
    }
    pub(super) async fn reserve_orders(
        &self,
        id: &str,
        orders: &[StoredOrder],
        intent: &str,
    ) -> Result<chain::TxObservation> {
        let mut record = self.budget_record(id)?;
        count(orders.iter().map(|o| o.order.unsigned.windows.len()).sum())?;
        for order in orders {
            ensure!(
                order.view.role == Role::Sender
                    && order.confirmed
                    && order.view.payment.budget_id.as_deref() == Some(id),
                "only explicitly confirmed sender orders can reserve this budget"
            );
            self.authorize_order_effect(order).await?;
        }
        let actor = orders.first().context("empty reservation batch")?;
        ensure!(
            orders
                .iter()
                .all(|order| order.network_id == actor.network_id
                    && order.authorized_by == actor.authorized_by),
            "batch managing authorization differs"
        );
        record.network_id = actor.network_id.clone();
        record.authorized_by = actor.authorized_by.clone();
        self.with_store(|store| store.put_value(&record_key(id), &serde_json::to_vec(&record)?))?;
        let reservations = orders
            .iter()
            .map(|o| Reservation {
                order: o.order.clone(),
                source_seals: o.source_seals.clone(),
                wallet_signature: o.wallet_signature.clone(),
            })
            .collect();
        self.chain()
            .await?
            .execute(
                self,
                &format!("{}/reserve-{intent}", group(id)),
                chain::Call::Budget(Box::new(BudgetAction::Reserve {
                    budget_id: parse_word(id)?,
                    reservations,
                })),
                &record.payer_gas_cap,
            )
            .await
    }
    pub(super) async fn flush_budget_holds(&self, id: &str) -> Result<()> {
        let Some(body) = self.with_store(|s| s.value(&format!("protected-flush:{id}")))? else {
            return Ok(());
        };
        let authorized: Vec<WindowRef> = serde_json::from_slice(&body)?;
        count(authorized.len())?;
        let chain = self.chain().await?;
        let position = chain.confirmed_position().await?;
        let snapshot = chain.budget_at(&position, parse_word(id)?.into()).await?;
        let record = self.register_budget(&snapshot, "0")?;
        let mut candidates = Vec::new();
        let mut orders = self.with_store(|s| s.orders(None))?;
        orders.retain(|o| {
            o.view.role == Role::Provider
                && o.accepted
                && o.view.payment.budget_id.as_deref() == Some(id)
                && authorized.iter().any(|w| w.order_id == o.view.order_id)
        });
        orders.sort_by_key(|o| {
            (
                o.order.unsigned.terms.priority_deadline,
                o.order.unsigned.terms.key_deadline,
            )
        });
        for order in &orders {
            let state = chain
                .order_at(&position, order.order.unsigned.terms.order_id()?)
                .await?;
            for (index, fact) in state.windows.iter().enumerate() {
                if candidates.len() == MAX_BATCH_WINDOWS {
                    break;
                }
                if fact.phase == 0 {
                    let w = WindowRef {
                        order_id: order.view.order_id.clone(),
                        index: index as u16,
                    };
                    if authorized.contains(&w)
                        && let Some(body) = self.with_store(|s| s.value(&hold_key(&w)))?
                    {
                        candidates.push(serde_json::from_slice::<HoldAuthorization>(&body)?);
                    }
                }
            }
        }
        chain.verify_position(&position).await?;
        if !candidates.is_empty() {
            let identity = hex::encode(hash(&serde_json::to_vec(&candidates)?));
            let result = chain
                .execute(
                    self,
                    &format!("{}/hold-{identity}", group(id)),
                    chain::Call::Budget(Box::new(BudgetAction::Hold {
                        budget_id: parse_word(id)?,
                        items: candidates,
                    })),
                    &record.provider_gas_cap,
                )
                .await?;
            if result.phase != "confirmed" {
                return Ok(());
            }
        }
        self.queue_publication(id, &orders).await
    }
    async fn authorize_budget_batch(&self, id: &str) -> Result<()> {
        let record = self.budget_record(id)?;
        ensure!(
            self.identity.device_id()? == record.terms.provider_peer,
            "only the fixed provider Node can flush its protected budget"
        );
        let mut orders = self.with_store(|s| s.orders(None))?;
        orders.retain(|o| {
            o.view.role == Role::Provider
                && o.accepted
                && o.view.payment.budget_id.as_deref() == Some(id)
        });
        orders.sort_by_key(|o| {
            (
                o.order.unsigned.terms.priority_deadline,
                o.order.unsigned.terms.key_deadline,
            )
        });
        let mut selected = Vec::new();
        for order in orders {
            let state = self.paid_state(&order.order).await?;
            let mut ready = Vec::new();
            for (index, fact) in state.windows.iter().enumerate() {
                if fact.phase == 3 {
                    continue;
                }
                let w = WindowRef {
                    order_id: order.view.order_id.clone(),
                    index: index as u16,
                };
                if self
                    .with_store(|s| s.value(&claim_window_key(&w)))?
                    .is_some()
                    || self.with_store(|s| s.value(&hold_key(&w)))?.is_none()
                    || !matches!(fact.phase, 0 | 1)
                {
                    ready.clear();
                    break;
                }
                ready.push(w);
            }
            if selected.len() + ready.len() <= MAX_BATCH_WINDOWS {
                selected.extend(ready);
            }
        }
        count(selected.len())?;
        self.with_store(|s| {
            s.put_value(
                &format!("protected-flush:{id}"),
                &serde_json::to_vec(&selected)?,
            )
        })
    }
    async fn queue_publication(&self, id: &str, orders: &[StoredOrder]) -> Result<()> {
        let policy = self
            .config()
            .publication_policy
            .context("protected publication requires an explicit local PublicationPolicy")?;
        policy.validate()?;
        let mut active = 0;
        for claim in self.claims()? {
            active += usize::from(claim.occupies_publication_window(super::now())?);
        }
        if active >= MAX_ACTIVE_CLAIMS {
            return Ok(());
        }
        let chain = self.chain().await?;
        let position = chain.confirmed_position().await?;
        let budget = chain.budget_at(&position, parse_word(id)?.into()).await?;
        let record = self.budget_record(id)?;
        ensure!(
            budget.terms == record.terms,
            "publication budget terms changed"
        );
        let mut items = Vec::new();
        let mut total = U256::ZERO;
        let mut cutoff = u64::MAX;
        let mut expected_earned = std::collections::BTreeMap::new();
        let publication_at = super::now().max(position.timestamp);
        for order in orders {
            let state = chain
                .order_at(&position, order.order.unsigned.terms.order_id()?)
                .await?;
            let needed: Vec<_> = state
                .windows
                .iter()
                .enumerate()
                .filter(|(_, w)| w.phase != 3)
                .collect();
            if needed.is_empty()
                || needed.iter().any(|(_, w)| w.phase != 1)
                || items.len() + needed.len() > MAX_BATCH_WINDOWS
            {
                continue;
            }
            let refs: Vec<_> = needed
                .iter()
                .map(|(i, _)| WindowRef {
                    order_id: order.view.order_id.clone(),
                    index: *i as u16,
                })
                .collect();
            let mut already_claimed = false;
            for w in &refs {
                already_claimed |= self
                    .with_store(|s| s.value(&claim_window_key(w)))?
                    .is_some();
            }
            if already_claimed {
                continue;
            }
            let before = total;
            for w in refs {
                let capsule = self.with_store(|s| s.object(&w.order_id, w.index, "capsule"))?;
                let spec = &order.order.unsigned.windows[w.index as usize];
                ensure!(
                    capsule.len() == 80 && hash(&capsule) == spec.capsule_hash,
                    "publication capsule differs from its reserved manifest"
                );
                total = total
                    .checked_add(U256::from_be_bytes(spec.delivery_fee.to_be_bytes()))
                    .and_then(|n| {
                        n.checked_add(U256::from_be_bytes(spec.reservation_fee.to_be_bytes()))
                    })
                    .context("claim amount overflow")?;
                items.push(Reveal { window: w, capsule });
            }
            let terms = &order.order.unsigned.terms;
            cutoff = cutoff.min(terms.key_deadline);
            if !state.bonus_earned
                && !state.bonus_refunded
                && terms.whole_order_bonus != vibeface_delivery::Amount::ZERO
                && publication_at <= terms.priority_deadline
            {
                total = total
                    .checked_add(U256::from_be_bytes(terms.whole_order_bonus.to_be_bytes()))
                    .context("claim bonus overflow")?;
                cutoff = cutoff.min(terms.priority_deadline);
            }
            expected_earned.insert(
                order.view.order_id.clone(),
                chain::amount(&state.earned)?
                    .checked_add(total - before)
                    .context("claim order total overflow")?
                    .to_string(),
            );
        }
        chain.verify_position(&position).await?;
        if items.is_empty() {
            return Ok(());
        }
        let terms = ClaimTerms {
            budget_id: id.into(),
            payee: record.terms.payee,
            items,
            economic_cutoff: cutoff,
            total_amount: total.to_string(),
            gas_cap: record.provider_gas_cap,
            policy,
            expected_earned,
        };
        let claim = PublicationClaim {
            id: hex::encode(hash(&serde_json::to_vec(&terms)?)),
            terms,
            state: ClaimState::PrivateReady,
            attempts: Vec::new(),
            risk: None,
            checked_at_ms: 0,
        };
        let mut values = vec![(claim.key(), serde_json::to_vec(&claim)?)];
        for item in &claim.terms.items {
            values.push((claim_window_key(&item.window), claim.id.as_bytes().to_vec()));
        }
        self.with_store(|s| {
            s.put_values(
                &values
                    .iter()
                    .map(|(key, body)| (key.as_str(), body.as_slice()))
                    .collect::<Vec<_>>(),
            )
        })
    }
    pub(super) async fn run_publications(&self, execute: bool) -> Result<()> {
        let Ok(_publication) = self.publication_operations.try_lock() else {
            return Ok(());
        };
        if !self.data_dir.join("delivery/service.sqlite3").exists() {
            return Ok(());
        }
        let mut claims = Vec::new();
        for claim in self.claims()? {
            claims.push((claim.occupies_publication_window(super::now())?, claim));
        }
        claims.sort_by_key(|(active, c)| {
            (
                !*active,
                c.state.first_possible_at().is_none(),
                if *active {
                    c.terms.economic_cutoff
                } else {
                    c.checked_at_ms.max(0) as u64
                },
            )
        });
        for (_, mut claim) in claims.into_iter().take(MAX_ACTIVE_CLAIMS + 1) {
            let result = tokio::time::timeout(
                std::time::Duration::from_secs(PUBLICATION_STEP_SECONDS),
                self.reconcile_publication(&mut claim, execute),
            )
            .await;
            match result {
                Ok(Ok(())) => {}
                Ok(Err(_)) => {
                    claim.risk = Some("publication reconciliation is unavailable; durable claim and attempts are preserved".into());
                    self.save_claim(&claim)?;
                }
                Err(_) => {
                    claim.risk = Some("publication step exceeded its local scheduling bound; availability assumptions are not met".into());
                    self.save_claim(&claim)?;
                }
            }
            claim.checked_at_ms = crate::runtime::now_ms();
            self.save_claim(&claim)?;
        }
        Ok(())
    }
    async fn reconcile_publication(
        &self,
        claim: &mut PublicationClaim,
        execute: bool,
    ) -> Result<()> {
        let chain = self.chain().await?;
        let record = self.budget_record(&claim.terms.budget_id)?;
        ensure!(
            record.terms.payee == claim.terms.payee
                && record.provider_gas_cap == claim.terms.gas_cap,
            "durable publication authorization changed"
        );
        let position = chain.confirmed_position().await?;
        let budget = chain
            .budget_at(&position, parse_word(&claim.terms.budget_id)?.into())
            .await?;
        ensure!(
            budget.terms == record.terms,
            "publication budget no longer matches its immutable terms"
        );
        let mut earned = true;
        let mut all_windows_earned = true;
        let mut live_held = true;
        for item in &claim.terms.items {
            let id = parse_word(&item.window.order_id)?;
            ensure!(
                chain.order_budget_at(&position, id).await? == parse_word(&claim.terms.budget_id)?,
                "publication window belongs to another budget"
            );
            let state = chain.order_at(&position, id).await?;
            let stored = self.with_store(|s| s.order(&item.window.order_id))?;
            ensure!(
                state.payer == record.terms.payer
                    && state.provider_wallet == claim.terms.payee
                    && chain
                        .public_values_at(&position, id, item.window.index, Relation::Source)
                        .await?
                        == Statement::window(
                            &stored.order.unsigned,
                            item.window.index,
                            Relation::Source
                        )?,
                "publication conditions differ from the reserved signed manifest"
            );
            let fact = state
                .windows
                .get(item.window.index as usize)
                .context("publication window disappeared")?;
            ensure!(
                hash(&item.capsule) == fact.capsule_hash,
                "durable capsule differs from its reserved window"
            );
            if fact.phase == 3 {
                ensure!(fact.capsule == item.capsule, "earned capsule changed");
            } else {
                earned = false;
                all_windows_earned = false;
                live_held &= fact.phase == 1;
            }
            let expected = claim
                .terms
                .expected_earned
                .get(&item.window.order_id)
                .context("claim has no immutable order amount")?;
            earned &= chain::amount(&state.earned)? >= chain::amount(expected)?;
        }
        chain.verify_position(&position).await?;
        let disclosed = claim.state.first_possible_at();
        if earned {
            claim.state = ClaimState::FinalEarned {
                first_possible_at: disclosed,
            };
            claim.risk = None;
            return self.save_claim(claim);
        }
        if all_windows_earned {
            claim.risk = Some("windows earned less than the authorized whole-order amount; original bonus conditions were missed".into());
            return self.save_claim(claim);
        }
        if matches!(claim.state, ClaimState::FinalEarned { .. }) {
            claim.state = match disclosed {
                Some(first_possible_at) => ClaimState::DisclosedOutstanding { first_possible_at },
                None => ClaimState::PrivateReady,
            };
        }
        let mut last = None;
        for id in &claim.attempts {
            let transaction = self.with_store(|s| s.transaction(id))?;
            let observed = chain.observe(&transaction).await?;
            self.with_store(|s| {
                s.observe_transaction(
                    id,
                    &observed.phase,
                    observed.block_number,
                    observed.block_hash.as_deref(),
                    observed.error.as_deref(),
                )
            })?;
            if observed.phase != "reverted" {
                last = Some((transaction, observed));
            }
        }
        let (chain_now, base_fee) = chain.publication_clock().await?;
        let now = super::now().max(chain_now);
        if now >= claim.terms.economic_cutoff {
            if disclosed.is_none() {
                claim.state = ClaimState::ExpiredUnrevealed;
            }
            claim.risk = Some("economic deadline passed; unearned payment cannot be promised, reconciliation continues".into());
            return self.save_claim(claim);
        }
        if !live_held {
            claim.risk = Some("original Hold/allocation is not canonical and live; no further recovery material is sent".into());
            return self.save_claim(claim);
        }
        let config = self.config();
        let enabled = execute
            && config.enabled
            && config.paid_enabled
            && config.publication_policy.as_ref() == Some(&claim.terms.policy)
            && config.provider_gas_cap == record.provider_gas_cap
            && config.chain.as_ref() == Some(chain.config());
        let unlocked = matches!(self.wallet.lock().map_err(|_| anyhow::anyhow!("delivery wallet lock failed"))?.status(), WalletStatus::Unlocked { address } if address == claim.terms.payee);
        if !enabled || !unlocked {
            claim.risk = Some("publication recovery paused: lab, paid policy or unlocked wallet authorization is unavailable".into());
            return self.save_claim(claim);
        }
        if !claim
            .terms
            .policy
            .disclosure_allowed(&claim.state, now, claim.terms.economic_cutoff)?
        {
            claim.risk = Some("first disclosure refused: full W+I+F+S margin is unavailable; signed bytes remain private".into());
            return self.save_claim(claim);
        }
        let wallet = chain::address(&claim.terms.payee)?;
        let transaction = if let Some((transaction, observed)) = last {
            if !matches!(observed.phase.as_str(), "unknown" | "reorged") {
                return Ok(());
            }
            let pending = chain.pending_nonce(wallet).await?;
            ensure!(
                pending == transaction.nonce
                    && chain.latest_nonce(wallet).await? == transaction.nonce,
                "unknown publication nonce outcome must be reconciled before any retry"
            );
            transaction
        } else {
            let id = format!(
                "{}/reveal-{}-{}",
                group(&claim.terms.budget_id),
                &claim.id[..16],
                claim.attempts.len()
            );
            let intent = chain.prepare_publication_intent(
                id.clone(),
                wallet,
                BudgetAction::Reveal {
                    budget_id: parse_word(&claim.terms.budget_id)?,
                    items: claim.terms.items.clone(),
                },
                claim.terms.gas_cap.clone(),
                &claim.terms.policy,
            )?;
            let pending = chain.pending_nonce(wallet).await?;
            ensure!(
                pending == chain.latest_nonce(wallet).await?,
                "publication requires an unobstructed contiguous nonce"
            );
            let existing = self.with_store(|s| s.find_transaction(&id))?;
            if existing.is_none() {
                ensure!(
                    self.with_store(|s| s.next_nonce_is_contiguous(
                        intent.chain_id,
                        &intent.wallet,
                        pending
                    ))?,
                    "publication is blocked by an earlier local nonce reservation"
                );
            }
            let cost = U256::from(intent.gas_limit)
                .checked_mul(chain::amount(&intent.max_fee_per_gas)?)
                .context("publication gas cost overflow")?;
            ensure!(
                chain::amount(&chain.native_balance(wallet).await?)? >= cost,
                "insufficient publication gas balance"
            );
            let fee = chain::amount(&claim.terms.policy.max_fee_per_gas)?;
            ensure!(
                base_fee
                    .checked_add(chain::amount(&claim.terms.policy.priority_fee_per_gas)?)
                    .is_some_and(|needed| needed <= fee),
                "fixed publication fee is below the local chain requirement"
            );
            self.authorize_spend(&id, chain.config()).await?;
            let current = self.config();
            ensure!(
                current.publication_policy.as_ref() == Some(&claim.terms.policy)
                    && current.chain.as_ref() == Some(chain.config()),
                "publication policy changed before reserving a signature"
            );
            let transaction = self.with_store(|s| {
                s.reserve_budget_transaction(
                    &intent,
                    pending,
                    &group(&claim.terms.budget_id),
                    &claim.terms.gas_cap,
                )
            })?;
            ensure!(
                transaction.nonce == pending,
                "publication nonce is not contiguous"
            );
            let signed = self
                .wallet
                .lock()
                .map_err(|_| anyhow::anyhow!("delivery wallet lock failed"))?
                .sign_transaction(chain.config(), &transaction.intent, transaction.nonce)?;
            claim.attempts.push(id.clone());
            self.with_store(|s| {
                s.sign_transaction_with_claim(
                    &id,
                    &signed.raw_tx,
                    &signed.tx_hash,
                    Some((&claim.key(), &serde_json::to_vec(&claim)?)),
                )
            })?;
            self.with_store(|s| s.transaction(&id))?
        };
        // Signing is private. Recheck immediately before the first possible E-bearing RPC.
        chain.verify_position(&position).await?;
        let (chain_now, latest_base_fee) = chain.publication_clock().await?;
        let now = super::now().max(chain_now);
        ensure!(
            claim.terms.policy.disclosure_allowed(
                &claim.state,
                now,
                claim.terms.economic_cutoff
            )?,
            "original publication disclosure conditions expired after signing"
        );
        let cost = U256::from(transaction.intent.gas_limit)
            .checked_mul(chain::amount(&transaction.intent.max_fee_per_gas)?)
            .context("publication gas cost overflow")?;
        let maximum_fee = chain::amount(&transaction.intent.max_fee_per_gas)?;
        ensure!(
            chain::amount(&chain.native_balance(wallet).await?)? >= cost
                && latest_base_fee
                    .checked_add(chain::amount(&transaction.intent.max_priority_fee_per_gas)?)
                    .is_some_and(|needed| needed <= maximum_fee),
            "publication gas balance or fixed fee is insufficient"
        );
        ensure!(
            chain.pending_nonce(wallet).await? == transaction.nonce
                && chain.latest_nonce(wallet).await? == transaction.nonce,
            "publication nonce changed before disclosure"
        );
        // The nonce/balance reads can consume the first-disclosure margin too.
        let (chain_now, _) = chain.publication_clock().await?;
        let now = super::now().max(chain_now);
        ensure!(
            claim.terms.policy.disclosure_allowed(
                &claim.state,
                now,
                claim.terms.economic_cutoff
            )?,
            "original disclosure gate expired at the RPC boundary"
        );
        self.authorize_spend(&transaction.intent.id, chain.config())
            .await?;
        let current = self.config();
        ensure!(
            current.publication_policy.as_ref() == Some(&claim.terms.policy)
                && current.chain.as_ref() == Some(chain.config())
                && current.provider_gas_cap == record.provider_gas_cap,
            "publication settings changed at the disclosure boundary"
        );
        ensure!(
            matches!(self.wallet.lock().map_err(|_| anyhow::anyhow!("delivery wallet lock failed"))?.status(), WalletStatus::Unlocked { address } if address == claim.terms.payee),
            "publication wallet was locked before disclosure"
        );
        if disclosed.is_none() {
            claim.state = ClaimState::DisclosedOutstanding {
                first_possible_at: now,
            };
        }
        claim.risk = Some("publication may be disclosed; awaiting canonical earned state".into());
        self.save_claim(claim)?;
        let signed = chain::SignedBytes {
            raw_tx: transaction
                .raw_tx
                .context("publication raw bytes are missing")?,
            tx_hash: transaction.tx_hash.context("publication hash is missing")?,
        };
        let _ = chain.broadcast(&signed).await;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    pub(super) fn policy() -> PublicationPolicy {
        PublicationPolicy {
            watchdog_seconds: 32,
            inclusion_seconds: 5,
            finality_seconds: 5,
            clock_skew_seconds: 2,
            max_fee_per_gas: "3000000000".into(),
            priority_fee_per_gas: "1000000000".into(),
        }
    }

    #[test]
    fn signed_but_private_requires_full_margin_while_disclosed_recovery_keeps_its_original_cutoff()
    -> Result<()> {
        let policy = policy();
        policy.validate()?;
        let private = ClaimState::PrivateReady;
        assert!(policy.disclosure_allowed(&private, 347, 400)?);
        assert!(!policy.disclosure_allowed(&private, 397, 400)?);
        let disclosed = ClaimState::DisclosedOutstanding {
            first_possible_at: 347,
        };
        assert!(
            policy.disclosure_allowed(&disclosed, 397, 400)?,
            "a legal earlier disclosure must recover near cutoff"
        );
        assert!(!policy.disclosure_allowed(&disclosed, 400, 400)?);
        let reorged_earned = ClaimState::FinalEarned {
            first_possible_at: Some(347),
        };
        assert!(policy.disclosure_allowed(&reorged_earned, 397, 400)?);
        let mut invalid = policy;
        invalid.watchdog_seconds = 1;
        assert!(invalid.validate().is_err());
        Ok(())
    }

    #[test]
    fn redacted_claim_status_contains_no_capsule_or_calldata() -> Result<()> {
        let capsule = vec![0x73; 80];
        let claim = PublicationClaim {
            id: "aa".repeat(32),
            terms: ClaimTerms {
                budget_id: "bb".repeat(32),
                payee: format!("0x{}", "cc".repeat(20)),
                items: vec![Reveal {
                    window: WindowRef {
                        order_id: "dd".repeat(32),
                        index: 0,
                    },
                    capsule: capsule.clone(),
                }],
                economic_cutoff: 400,
                total_amount: "17".into(),
                gas_cap: "10000000000000000".into(),
                policy: policy(),
                expected_earned: std::collections::BTreeMap::new(),
            },
            state: ClaimState::PrivateReady,
            attempts: vec!["budget-bb/reveal-aa-0".into()],
            risk: None,
            checked_at_ms: 0,
        };
        let value = claim.summary();
        assert_eq!(value["state"], "private_ready");
        assert_eq!(value["total_amount"], "17");
        let public = serde_json::to_string(&value)?;
        assert!(
            !public.contains("capsule")
                && !public.contains("calldata")
                && !public.contains(&hex::encode(capsule))
        );
        let mut restored: PublicationClaim = serde_json::from_slice(&serde_json::to_vec(&claim)?)?;
        assert_eq!(restored.terms, claim.terms);
        assert!(restored.occupies_publication_window(347)?);
        restored.state = ClaimState::FinalEarned {
            first_possible_at: Some(347),
        };
        let until = 347 + restored.terms.policy.margin()?;
        assert!(restored.occupies_publication_window(until - 1)?);
        assert!(!restored.occupies_publication_window(until)?);
        restored.state = ClaimState::DisclosedOutstanding {
            first_possible_at: 347,
        };
        assert!(restored.occupies_publication_window(until + 1)?);
        Ok(())
    }
}
