//! The receipt relation for paid session delivery. Funding, time and spent orders are chain state.

use alloc::vec::Vec;
use serde::{Deserialize, Serialize};

use crate::{Amount, Error, PeerSignature, Result, Word, hash, verify_peer};

pub const SESSION_SETTLEMENT_RECEIPT_DOMAIN: &str = "bot-session-paid-stored-v1";
pub const MAX_SESSION_SETTLEMENT_ORDERS: usize = 16;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionSettlementOrder {
    /// Domain of the chain and escrow instance.
    pub deployment: Word,
    pub budget: Word,
    /// An opaque logical delivery identifier, fixed before the first upload and reused on retry.
    pub delivery: Word,
    pub receiver: Word,
    pub relay: Word,
    /// Commitment to the exact sealed parent event, including its sealed attachment list.
    pub object: Word,
    pub amount: Amount,
    pub claim_until_ms: u64,
}

impl SessionSettlementOrder {
    pub fn order_id(&self) -> Result<Word> {
        if [self.deployment, self.budget, self.delivery].contains(&[0; 32]) {
            return Err(Error("invalid session settlement order identity"));
        }
        Ok(hash(
            &[
                b"vibeface-session-order-v1".as_slice(),
                &self.deployment,
                &self.budget,
                &self.delivery,
            ]
            .concat(),
        ))
    }

    /// The recipient signs this payload only after verifying and durably storing the object.
    /// The payer must have authorized this same commitment in the budget reservation.
    pub fn commitment(&self) -> Result<Word> {
        if [self.receiver, self.relay, self.object].contains(&[0; 32])
            || self.receiver == self.relay
            || self.amount == Amount::ZERO
            || self.claim_until_ms == 0
        {
            return Err(Error("invalid session settlement order terms"));
        }
        Ok(hash(
            &[
                b"vibeface-session-settlement-terms-v1".as_slice(),
                &self.order_id()?,
                &self.receiver,
                &self.relay,
                &self.object,
                &self.amount.to_be_bytes(),
                &self.claim_until_ms.to_be_bytes(),
            ]
            .concat(),
        ))
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionSettlementClaim {
    pub order: SessionSettlementOrder,
    pub receipt: PeerSignature,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionSettlementBatch {
    pub deployment: Word,
    pub budget: Word,
    pub relay: Word,
    /// Hash of the ordered reservation commitments, with the batch domain prefix.
    pub reservations_hash: Word,
    pub order_count: u8,
    pub amount: Amount,
    pub claim_until_ms: u64,
}

/// Validates one batch for a single budget and relay. Orders must be strictly sorted by ID.
/// The settlement verifier must recompute the reservation hash from funded, unspent orders,
/// reject expired batches using chain time, and consume the individual order IDs atomically.
pub fn verify_session_settlement(
    claims: &[SessionSettlementClaim],
) -> Result<SessionSettlementBatch> {
    let Some(first) = claims
        .first()
        .filter(|_| claims.len() <= MAX_SESSION_SETTLEMENT_ORDERS)
    else {
        return Err(Error("invalid session settlement batch size"));
    };
    let first = &first.order;
    let mut total = Amount::ZERO;
    let mut earliest = u64::MAX;
    let mut previous = None;
    let domain = b"vibeface-session-settlement-batch-v1";
    let mut commitments = Vec::with_capacity(domain.len() + 32 * claims.len());
    commitments.extend_from_slice(domain);
    for claim in claims {
        let order = &claim.order;
        if (order.deployment, order.budget, order.relay)
            != (first.deployment, first.budget, first.relay)
        {
            return Err(Error("session settlement batch crosses budgets or relays"));
        }
        let id = order.order_id()?;
        if previous.is_some_and(|previous| previous >= id) {
            return Err(Error("session settlement orders must be unique and sorted"));
        }
        previous = Some(id);
        let commitment = order.commitment()?;
        verify_peer(
            &order.receiver,
            SESSION_SETTLEMENT_RECEIPT_DOMAIN,
            &commitment,
            &claim.receipt,
        )?;
        total = total.checked_add(order.amount)?;
        earliest = earliest.min(order.claim_until_ms);
        commitments.extend_from_slice(&commitment);
    }
    Ok(SessionSettlementBatch {
        deployment: first.deployment,
        budget: first.budget,
        relay: first.relay,
        reservations_hash: hash(&commitments),
        order_count: claims.len() as u8,
        amount: total,
        claim_until_ms: earliest,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::signing_message;
    use ed25519_dalek::{Signer, SigningKey};

    fn claim(delivery: u8, amount: u64, deadline: u64) -> SessionSettlementClaim {
        let receiver = SigningKey::from_bytes(&[42; 32]);
        sign(
            SessionSettlementOrder {
                deployment: [1; 32],
                budget: [2; 32],
                delivery: [delivery; 32],
                receiver: receiver.verifying_key().to_bytes(),
                relay: [4; 32],
                object: [5; 32],
                amount: Amount::from_u64(amount),
                claim_until_ms: deadline,
            },
            &receiver,
        )
    }

    fn sign(order: SessionSettlementOrder, key: &SigningKey) -> SessionSettlementClaim {
        let commitment = order.commitment().unwrap();
        let payload = signing_message(SESSION_SETTLEMENT_RECEIPT_DOMAIN, &commitment).unwrap();
        SessionSettlementClaim {
            order,
            receipt: PeerSignature::from_bytes(key.sign(&payload).to_bytes()),
        }
    }

    #[test]
    fn batch_requires_the_exact_receiver_signed_terms() {
        let mut claims = [claim(7, 4, 100), claim(8, 5, 80)];
        claims.sort_by_key(|claim| claim.order.order_id().unwrap());
        let batch = verify_session_settlement(&claims).unwrap();
        assert_eq!(batch.amount, Amount::from_u64(9));
        assert_eq!(batch.claim_until_ms, 80);
        assert_eq!(batch.order_count, 2);
        let first = &claims[0];
        let mut unauthorized = first.clone();
        unauthorized.order.amount = Amount::from_u64(50);
        assert!(verify_session_settlement(&[unauthorized]).is_err());
        let mut unauthorized = first.clone();
        unauthorized.order.deployment = [9; 32];
        assert!(verify_session_settlement(&[unauthorized]).is_err());
        let unauthorized = sign(first.order.clone(), &SigningKey::from_bytes(&[43; 32]));
        assert!(verify_session_settlement(&[unauthorized]).is_err());
        let mut unauthorized = first.clone();
        unauthorized.order.relay = [9; 32];
        assert!(verify_session_settlement(&[unauthorized]).is_err());
        claims.reverse();
        assert!(verify_session_settlement(&claims).is_err());
    }

    #[test]
    fn batch_rejects_individually_valid_claims_from_another_budget_and_total_overflow() {
        let first = claim(7, 4, 100);
        let mut other = claim(8, 5, 100).order;
        other.budget = [9; 32];
        let key = SigningKey::from_bytes(&[42; 32]);
        let mut mixed = [first.clone(), sign(other.clone(), &key)];
        for claim in &mixed {
            assert!(verify_session_settlement(core::slice::from_ref(claim)).is_ok());
        }
        mixed.sort_by_key(|claim| claim.order.order_id().unwrap());
        assert!(verify_session_settlement(&mixed).is_err());

        other.budget = first.order.budget;
        other.amount = Amount::from_be_bytes([u8::MAX; 32]);
        let mut overflowing = [first, sign(other, &key)];
        overflowing.sort_by_key(|claim| claim.order.order_id().unwrap());
        assert!(verify_session_settlement(&overflowing).is_err());
    }

    #[test]
    fn reencryption_keeps_order_id_and_cannot_be_counted_twice() {
        let original = claim(7, 4, 100);
        let mut retried_order = original.order.clone();
        retried_order.object = [9; 32];
        let retried = sign(retried_order, &SigningKey::from_bytes(&[42; 32]));
        assert_eq!(original.order.order_id(), retried.order.order_id());
        assert_ne!(original.order.commitment(), retried.order.commitment());
        assert!(verify_session_settlement(core::slice::from_ref(&retried)).is_ok());
        assert!(verify_session_settlement(&[original, retried]).is_err());
    }
}
