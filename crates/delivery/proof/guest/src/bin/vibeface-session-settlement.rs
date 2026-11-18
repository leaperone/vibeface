#![no_main]

use risc0_zkvm::guest::env;
use vibeface_delivery::{SessionSettlementClaim, verify_session_settlement};

risc0_zkvm::guest::entry!(main);

fn main() {
    let claims: Vec<SessionSettlementClaim> = env::read();
    let batch = verify_session_settlement(&claims).expect("session receipt batch rejected");
    env::commit_slice(&batch.journal());
}
