#![no_main]

use risc0_zkvm::guest::env;
use vibeface_delivery::{ProofInput, verify_relation};

risc0_zkvm::guest::entry!(main);

fn main() {
    let input: ProofInput = env::read();
    verify_relation(&input.profile, &input.statement, &input.witness)
        .expect("delivery relation rejected");
    env::commit_slice(input.statement.abi_bytes());
}
