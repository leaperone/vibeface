use super::{
    MAX_RECEIPT_INPUT, checked_receipt_for, decode_receipt, groth16_seal, local_prover,
    new_receipt_path, prove_outputs, publish_receipt, read_stdin,
};
use anyhow::{Context, Result, bail, ensure};
use risc0_zkvm::{Executor, ExecutorEnv, InnerReceipt, Prover, ProverOpts, Receipt, sha::Digest};
use serde_json::json;
use std::path::Path;
use vibeface_delivery::{SessionSettlementClaim, hash, verify_session_settlement};
use vibeface_delivery_methods::{
    VIBEFACE_SESSION_SETTLEMENT_ELF as ELF, VIBEFACE_SESSION_SETTLEMENT_ID as IMAGE,
};

const JOURNAL_BYTES: usize = 8 * 32;
const MAX_INPUT: u64 = 64 * 1024;
const MAX_CYCLES: u64 = 64_000_000;

fn public_receipt(receipt: &Receipt, journal: &[u8]) -> Result<serde_json::Value> {
    checked_receipt_for(receipt, journal, IMAGE)?;
    Ok(json!({
        "kind": "groth16",
        "imageId": hex::encode(Digest::from(IMAGE).as_bytes()),
        "journal": hex::encode(journal),
        "journalHash": hex::encode(hash(journal)),
        "seal": hex::encode(groth16_seal(receipt)?),
    }))
}

fn expected_journal(value: &str) -> Result<Vec<u8>> {
    ensure!(
        value.len() == 2 * JOURNAL_BYTES,
        "invalid session journal size"
    );
    let journal = hex::decode(value).context("invalid session journal hex")?;
    ensure!(
        journal[..32] == hash(b"vibeface-session-settlement-journal-v1"),
        "invalid session journal domain"
    );
    Ok(journal)
}

pub(super) fn run(command: &str, args: &[String]) -> Result<()> {
    match command {
        "session-info" if args.len() == 1 => {
            println!(
                "{}",
                json!({
                    "imageId": hex::encode(Digest::from(IMAGE).as_bytes()),
                    "elfSha256": hex::encode(hash(ELF)),
                    "journalBytes": JOURNAL_BYTES,
                })
            );
            return Ok(());
        }
        "session-verify" if args.len() == 2 => {
            let journal = expected_journal(&args[1])?;
            let receipt = decode_receipt(&read_stdin(MAX_RECEIPT_INPUT)?)?;
            println!("{}", public_receipt(&receipt, &journal)?);
            return Ok(());
        }
        "session-compress" if args.len() == 3 => {
            let output = new_receipt_path(Path::new(&args[1]))?;
            let journal = expected_journal(&args[2])?;
            let succinct = decode_receipt(&read_stdin(MAX_RECEIPT_INPUT)?)?;
            ensure!(
                matches!(succinct.inner, InnerReceipt::Succinct(_)),
                "genuine succinct receipt required"
            );
            checked_receipt_for(&succinct, &journal, IMAGE)?;
            let receipt = local_prover()?.compress(&ProverOpts::groth16(), &succinct)?;
            let public = public_receipt(&receipt, &journal)?;
            publish_receipt(&output, &bincode::serialize(&receipt)?)?;
            println!("{public}");
            return Ok(());
        }
        "session-execute" if args.len() == 1 => {}
        "session-prove" => {}
        _ => bail!("invalid session prover command or arguments"),
    }
    let outputs = if command == "session-prove" {
        let (output, succinct) = prove_outputs(args)?;
        let output = new_receipt_path(output)?;
        let succinct = succinct.map(new_receipt_path).transpose()?;
        ensure!(
            succinct.as_ref() != Some(&output),
            "receipt paths must differ"
        );
        Some((output, succinct))
    } else {
        None
    };
    let claims: Vec<SessionSettlementClaim> = serde_json::from_slice(&read_stdin(MAX_INPUT)?)
        .map_err(|_| anyhow::anyhow!("invalid private session proof input"))?;
    let journal = verify_session_settlement(&claims)?.journal();
    let env = ExecutorEnv::builder()
        .segment_limit_po2(19)
        .session_limit(Some(MAX_CYCLES))
        .write(&claims)?
        .build()?;
    let prover = local_prover()?;
    if let Some((output, succinct)) = outputs {
        let info = prover.prove_with_opts(env, ELF, &ProverOpts::succinct())?;
        ensure!(
            matches!(info.receipt.inner, InnerReceipt::Succinct(_)),
            "genuine succinct receipt required"
        );
        checked_receipt_for(&info.receipt, &journal, IMAGE)?;
        if let Some(path) = succinct {
            let bytes = bincode::serialize(&info.receipt)?;
            ensure!(
                bytes.len() as u64 <= MAX_RECEIPT_INPUT,
                "succinct receipt exceeds local limit"
            );
            publish_receipt(&path, &bytes)?;
        }
        let receipt = prover.compress(&ProverOpts::groth16(), &info.receipt)?;
        let public = public_receipt(&receipt, &journal)?;
        publish_receipt(&output, &bincode::serialize(&receipt)?)?;
        println!("{public}");
    } else {
        let result = prover.execute(env, ELF)?;
        ensure!(
            result.journal.bytes == journal,
            "session execution journal mismatch"
        );
        let cycles: u64 = result
            .segments
            .iter()
            .map(|part| u64::from(part.cycles))
            .sum();
        ensure!(cycles <= MAX_CYCLES, "session execution exceeds cap");
        println!(
            "{}",
            json!({"journalHash": hex::encode(hash(&journal)), "cycles": cycles, "proved": false})
        );
    }
    Ok(())
}
