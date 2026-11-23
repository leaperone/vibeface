//! Pure paid-budget ABI codec.
//!
//! This module is deliberately independent from `Delivery`, stores, clocks, and RPC.  The
//! context contains only the deployment facts needed to validate a paid call.

use super::{chain, chain::abi, types::parse_word};
use alloy_primitives::{Address, B256, Bytes, U256};
use alloy_sol_types::{SolCall, SolValue};
use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use vibeface_delivery::{Order, Relation, Statement, Word};

pub const MAX_BATCH_WINDOWS: usize = 2;

#[derive(Clone, Debug)]
pub struct CodecContext {
    pub chain_id: u64,
    pub escrow: Address,
    pub asset: Address,
    pub profile: vibeface_delivery::ProofProfile,
    pub signer: Address,
}

impl CodecContext {
    pub fn from_config(config: &super::chain::ChainConfig, signer: Address) -> Result<Self> {
        Ok(Self {
            chain_id: config.chain_id,
            escrow: chain::address(&config.escrow)?,
            asset: chain::address(&config.asset)?,
            profile: config.profile.clone(),
            signer,
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ActionKind {
    Open,
    Reserve,
    Hold,
    Reveal,
    Withdraw,
    Close,
    Refund,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EncodedCall {
    pub target: Address,
    pub calldata: Vec<u8>,
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

pub(crate) fn count(count: usize) -> Result<()> {
    ensure!(
        (1..=MAX_BATCH_WINDOWS).contains(&count),
        "a protected batch contains at most two windows"
    );
    Ok(())
}

fn window(window: &WindowRef) -> Result<abi::WindowRef> {
    let order_id = parse_word(&window.order_id)?;
    chain::validate_order_index(order_id, window.index)?;
    ensure!(window.index < 2, "protected window exceeds Node profile");
    Ok(abi::WindowRef {
        orderId: order_id.into(),
        index: window.index,
    })
}

fn validate_statement(ctx: &CodecContext, input: &abi::DeliveryStatement) -> Result<()> {
    Statement::from_bytes(&input.abi_encode())?;
    ensure!(
        input.chainId == U256::from(ctx.chain_id)
            && input.escrow == ctx.escrow
            && input.profileId.as_slice() == ctx.profile.digest()?.as_slice()
            && input.guestImageId.as_slice() == ctx.profile.guest_image_id,
        "delivery statement belongs to another deployment"
    );
    Ok(())
}

fn source(ctx: &CodecContext, r: Reservation) -> Result<abi::Reservation> {
    r.order.verify()?;
    ctx.profile.check_order(&r.order.unsigned)?;
    ensure!(
        r.order.unsigned.terms.payer == ctx.signer.into_array()
            && r.order.unsigned.terms.asset == ctx.asset.into_array()
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
    validate_statement(ctx, &input)?;
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

pub fn encode_action(ctx: &CodecContext, action: BudgetAction) -> Result<EncodedCall> {
    let input = match action {
        BudgetAction::Open {
            salt,
            payee,
            provider_peer,
            deposit,
        } => abi::openBudgetCall {
            salt: salt.into(),
            payee,
            providerPeer: provider_peer.into(),
            asset: ctx.asset,
            deposit: chain::amount(&deposit)?,
        }
        .abi_encode(),
        BudgetAction::Reserve {
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
                    .map(|r| source(ctx, r))
                    .collect::<Result<_>>()?,
            }
            .abi_encode()
        }
        BudgetAction::Hold { budget_id, items } => {
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
        BudgetAction::Reveal { budget_id, items } => {
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
        BudgetAction::Withdraw { budget_id } => abi::withdrawBudgetCall {
            id: budget_id.into(),
        }
        .abi_encode(),
        BudgetAction::Close { budget_id } => abi::closeBudgetCall {
            id: budget_id.into(),
        }
        .abi_encode(),
        BudgetAction::Refund { budget_id, windows } => {
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
    validate_calldata(ctx, &input)?;
    Ok(EncodedCall {
        target: ctx.escrow,
        calldata: input,
    })
}

pub fn decode_call(ctx: &CodecContext, target: Address, input: &[u8]) -> Result<ActionKind> {
    ensure!(
        target == ctx.escrow,
        "delivery wallet refuses an unknown recipient"
    );
    validate_calldata(ctx, input)
}

fn validate_calldata(ctx: &CodecContext, input: &[u8]) -> Result<ActionKind> {
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
    let result = match selector {
        s if s == abi::openBudgetCall::SELECTOR => {
            let c = exact!(openBudgetCall);
            ensure!(
                c.salt != B256::ZERO
                    && c.providerPeer != B256::ZERO
                    && c.payee != Address::ZERO
                    && c.payee != ctx.signer
                    && c.payee != ctx.escrow
                    && c.asset == ctx.asset
                    && c.deposit > U256::ZERO,
                "invalid fixed protected budget terms"
            );
            (ActionKind::Open, c.salt)
        }
        s if s == abi::reserveBatchCall::SELECTOR => {
            let c = exact!(reserveBatchCall);
            count(c.reservations.iter().map(|r| r.specs.len()).sum())?;
            for r in c.reservations {
                validate_statement(ctx, &r.input)?;
                ensure!(
                    r.input.relation == 2
                        && r.input.acceptance == 0
                        && r.input.payer == ctx.signer
                        && r.input.asset == ctx.asset
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
            (ActionKind::Reserve, c.budgetId)
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
            (ActionKind::Hold, c.budgetId)
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
            (ActionKind::Reveal, c.budgetId)
        }
        s if s == abi::withdrawBudgetCall::SELECTOR => {
            let c = exact!(withdrawBudgetCall);
            (ActionKind::Withdraw, c.id)
        }
        s if s == abi::closeBudgetCall::SELECTOR => {
            let c = exact!(closeBudgetCall);
            (ActionKind::Close, c.id)
        }
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
            (ActionKind::Refund, c.id)
        }
        _ => bail!("delivery wallet refuses an unknown or unprotected contract action"),
    };
    if result.0 != ActionKind::Open {
        ensure!(result.1 != B256::ZERO, "protected budget id is zero");
    }
    Ok(result.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn context() -> CodecContext {
        CodecContext {
            chain_id: 31_337,
            escrow: Address::repeat_byte(0x11),
            asset: Address::repeat_byte(0x22),
            profile: serde_json::from_str(include_str!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/../../crates/delivery/proof/profile.json"
            )))
            .expect("pinned delivery profile"),
            signer: Address::repeat_byte(0x33),
        }
    }

    #[test]
    fn open_call_round_trips_through_the_single_codec_boundary() -> Result<()> {
        let ctx = context();
        let encoded = encode_action(
            &ctx,
            BudgetAction::Open {
                salt: [1; 32],
                payee: Address::repeat_byte(0x44),
                provider_peer: [2; 32],
                deposit: "1000000".into(),
            },
        )?;
        let decoded = decode_call(&ctx, encoded.target, &encoded.calldata)?;
        assert_eq!(decoded, ActionKind::Open);
        Ok(())
    }

    #[test]
    fn codec_rejects_unknown_noncanonical_and_zero_budget_calls() -> Result<()> {
        let ctx = context();
        assert!(decode_call(&ctx, ctx.escrow, &[0xde, 0xad, 0xbe, 0xef]).is_err());

        let encoded = encode_action(
            &ctx,
            BudgetAction::Reveal {
                budget_id: [3; 32],
                items: vec![Reveal {
                    window: WindowRef {
                        order_id: "04".repeat(32),
                        index: 0,
                    },
                    capsule: vec![7; 80],
                }],
            },
        )?;
        let mut trailing = encoded.calldata.clone();
        trailing.push(0);
        assert!(decode_call(&ctx, encoded.target, &trailing).is_err());

        assert!(
            encode_action(
                &ctx,
                BudgetAction::Reveal {
                    budget_id: [0; 32],
                    items: vec![Reveal {
                        window: WindowRef {
                            order_id: "04".repeat(32),
                            index: 0,
                        },
                        capsule: vec![7; 80],
                    }],
                },
            )
            .is_err()
        );
        Ok(())
    }
}
