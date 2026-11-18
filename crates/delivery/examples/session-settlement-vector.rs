//! Synthetic public vectors for the loopback EVM accounting fixture.

use ed25519_dalek::{Signer, SigningKey};
use serde_json::json;
use vibeface_delivery::{
    Amount, PeerSignature, SESSION_SETTLEMENT_RECEIPT_DOMAIN, SessionSettlementClaim,
    SessionSettlementOrder, Word, hash, signing_message, verify_session_settlement,
};

fn main() {
    let args: Vec<_> = std::env::args().skip(1).collect();
    let word = |i: usize| -> Word {
        hex::decode(args[i].strip_prefix("0x").unwrap())
            .unwrap()
            .try_into()
            .unwrap()
    };
    let deadline: u64 = args[3].parse().unwrap();
    let key = SigningKey::from_bytes(&[42; 32]);
    let claims = |indices: &[u8]| {
        let mut claims: Vec<_> = indices
            .iter()
            .map(|i| {
                let order = SessionSettlementOrder {
                    deployment: word(0),
                    budget: word(1),
                    relay: word(2),
                    delivery: [*i; 32],
                    receiver: key.verifying_key().to_bytes(),
                    object: [77 + i; 32],
                    amount: Amount::from_u64(u64::from(*i)),
                    claim_until_ms: deadline + 1000 * u64::from(*i),
                };
                let payload = signing_message(
                    SESSION_SETTLEMENT_RECEIPT_DOMAIN,
                    &order.commitment().unwrap(),
                )
                .unwrap();
                let receipt = PeerSignature::from_bytes(key.sign(&payload).to_bytes());
                SessionSettlementClaim { order, receipt }
            })
            .collect();
        claims.sort_by_key(|claim| claim.order.order_id().unwrap());
        claims
    };
    let hex = |word: Word| format!("0x{}", hex::encode(word));
    let vector = |claims: &[SessionSettlementClaim]| {
        let batch = verify_session_settlement(claims).unwrap();
        json!({
            "orders": claims.iter().map(|claim| {
                let o = &claim.order;
                json!({
                    "id": hex(o.order_id().unwrap()),
                    "delivery": hex(o.delivery),
                    "context": hex(o.receipt_context()),
                    "commitment": hex(o.commitment().unwrap()),
                    "amount": o.amount.to_decimal_string(),
                    "deadline": o.claim_until_ms,
                })
            }).collect::<Vec<_>>(),
            "journalDigest": hex(hash(&batch.journal())),
        })
    };
    println!(
        "{}",
        json!({"paid": vector(&claims(&[2,3])), "expire": vector(&claims(&[4]))})
    );
}
