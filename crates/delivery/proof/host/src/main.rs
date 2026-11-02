use anyhow::{Context, Result, bail, ensure};
use bincode::Options;
use risc0_zkvm::{
    Executor, ExecutorEnv, ExternalProver, InnerReceipt, Prover, ProverOpts, Receipt, sha::Digest,
};
use serde_json::json;
use std::io::{Read, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use vibeface_delivery::{ProofInput, hash, schema_hash};
use vibeface_delivery_methods::{VIBEFACE_DELIVERY_GUEST_ELF, VIBEFACE_DELIVERY_GUEST_ID};
use zeroize::Zeroizing;

const MAX_PRIVATE_INPUT: u64 = 1024 * 1024;
const MAX_RECEIPT_INPUT: u64 = 16 * 1024 * 1024;
static RECEIPT_SEQUENCE: AtomicU64 = AtomicU64::new(0);

fn read_stdin(limit: u64) -> Result<Zeroizing<Vec<u8>>> {
    let mut bytes = Zeroizing::new(Vec::new());
    std::io::stdin().take(limit + 1).read_to_end(&mut bytes)?;
    ensure!(bytes.len() as u64 <= limit, "input exceeds local limit");
    Ok(bytes)
}

fn local_prover() -> Result<ExternalProver> {
    let path = std::env::var_os("RISC0_SERVER_PATH")
        .context("set RISC0_SERVER_PATH to a trusted local r0vm")?;
    ensure!(
        std::path::Path::new(&path).is_absolute(),
        "r0vm path must be absolute"
    );
    Ok(ExternalProver::new("delivery-local-ipc", path))
}

fn image_id() -> Vec<u8> {
    Digest::from(VIBEFACE_DELIVERY_GUEST_ID).as_bytes().to_vec()
}

fn checked_receipt(receipt: &Receipt, journal: &[u8]) -> Result<()> {
    ensure!(
        matches!(
            receipt.inner,
            InnerReceipt::Succinct(_) | InnerReceipt::Groth16(_)
        ),
        "genuine succinct or Groth16 receipt required"
    );
    receipt.verify(VIBEFACE_DELIVERY_GUEST_ID)?;
    ensure!(receipt.journal.bytes == journal, "receipt journal mismatch");
    Ok(())
}

fn checked_succinct_receipt(receipt: &Receipt, journal: &[u8]) -> Result<()> {
    ensure!(
        matches!(receipt.inner, InnerReceipt::Succinct(_)),
        "genuine succinct receipt required"
    );
    checked_receipt(receipt, journal)
}

fn decode_receipt(bytes: &[u8]) -> Result<Receipt> {
    ensure!(
        bytes.len() as u64 <= MAX_RECEIPT_INPUT,
        "receipt exceeds local limit"
    );
    Ok(bincode::DefaultOptions::new()
        .with_fixint_encoding()
        .with_limit(MAX_RECEIPT_INPUT)
        .reject_trailing_bytes()
        .deserialize(bytes)?)
}

fn expected_statement(value: &str) -> Result<vibeface_delivery::Statement> {
    let value = value.strip_prefix("0x").unwrap_or(value);
    ensure!(
        value.len() == vibeface_delivery::STATEMENT_BYTES * 2,
        "expected journal must have the exact statement length"
    );
    Ok(vibeface_delivery::Statement::from_bytes(&hex::decode(
        value,
    )?)?)
}

fn prove_outputs(args: &[String]) -> Result<(&Path, Option<&Path>)> {
    ensure!(
        args.len() == 2 || (args.len() == 4 && args[2] == "--succinct-receipt"),
        "prove requires a new receipt path and optional --succinct-receipt <new-path>"
    );
    Ok((
        Path::new(&args[1]),
        (args.len() == 4).then(|| Path::new(&args[3])),
    ))
}

fn receipt_parent(path: &Path) -> Result<&Path> {
    path.file_name()
        .context("receipt output requires a file name")?;
    let parent = path
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let metadata = std::fs::symlink_metadata(parent)?;
    ensure!(
        metadata.is_dir() && !metadata.file_type().is_symlink(),
        "receipt directory must not be a symlink"
    );
    Ok(parent)
}

fn new_receipt_path(path: &Path) -> Result<std::path::PathBuf> {
    let parent = receipt_parent(path)?;
    match std::fs::symlink_metadata(path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
        Ok(_) => bail!("receipt already exists; recover it with verify or compress"),
    }
    Ok(std::fs::canonicalize(parent)?.join(
        path.file_name()
            .context("receipt output requires a file name")?,
    ))
}

fn groth16_seal(receipt: &Receipt) -> Result<Vec<u8>> {
    match &receipt.inner {
        InnerReceipt::Groth16(inner) => {
            ensure!(inner.seal.len() == 256, "Groth16 seal must be 256 bytes");
            let mut seal = inner.verifier_parameters.as_bytes()[..4].to_vec();
            seal.extend_from_slice(&inner.seal);
            Ok(seal)
        }
        _ => bail!("Groth16 receipt required"),
    }
}

fn publish_receipt(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = receipt_parent(path)?;
    let name = path
        .file_name()
        .context("receipt output requires a file name")?;
    let (temporary, mut file) = loop {
        let sequence = RECEIPT_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let mut temporary_name = std::ffi::OsString::from(".");
        temporary_name.push(name);
        temporary_name.push(format!(".{}.{sequence}.tmp", std::process::id()));
        let temporary = parent.join(temporary_name);
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temporary)
        {
            Ok(file) => break (temporary, file),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error.into()),
        }
    };
    let result = (|| -> Result<()> {
        file.write_all(bytes)?;
        file.sync_all()?;
        drop(file);
        // A hard link publishes a complete file atomically without overwriting another result.
        std::fs::hard_link(&temporary, path)?;
        std::fs::remove_file(&temporary)?;
        std::fs::File::open(parent)?.sync_all()?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    result
}

fn main() -> Result<()> {
    // R0VM inherits this mask so its tempfile directories and files stay private.
    unsafe { libc::umask(0o077) };
    ensure!(
        std::env::var_os("RISC0_DEV_MODE").is_none(),
        "dev mode is forbidden"
    );
    let args: Vec<_> = std::env::args().skip(1).collect();
    let command = args.first().map(String::as_str).unwrap_or("info");
    if command == "info" {
        println!(
            "{}",
            json!({"imageId":hex::encode(image_id()),"schemaHash":hex::encode(schema_hash()),
            "elfSha256":hex::encode(hash(VIBEFACE_DELIVERY_GUEST_ELF)),"statementBytes":vibeface_delivery::STATEMENT_BYTES})
        );
        return Ok(());
    }
    if command == "verify" {
        ensure!(
            args.len() == 2,
            "verify requires the expected public journal hex"
        );
        let statement = expected_statement(&args[1])?;
        let receipt = decode_receipt(&read_stdin(MAX_RECEIPT_INPUT)?)?;
        checked_receipt(&receipt, statement.abi_bytes())?;
        let seal = groth16_seal(&receipt)?;
        println!(
            "{}",
            json!({"verified":true,"kind":"groth16","seal":hex::encode(seal),"imageId":hex::encode(image_id()),"journalHash":hex::encode(statement.digest())})
        );
        return Ok(());
    }
    if command == "compress" {
        ensure!(
            args.len() == 3,
            "compress requires a new receipt path and the expected public journal hex"
        );
        let output = new_receipt_path(Path::new(&args[1]))?;
        let statement = expected_statement(&args[2])?;
        let succinct = decode_receipt(&read_stdin(MAX_RECEIPT_INPUT)?)?;
        checked_succinct_receipt(&succinct, statement.abi_bytes())?;
        let receipt = local_prover()?.compress(&ProverOpts::groth16(), &succinct)?;
        checked_receipt(&receipt, statement.abi_bytes())?;
        let seal = groth16_seal(&receipt)?;
        publish_receipt(&output, &bincode::serialize(&receipt)?)?;
        println!(
            "{}",
            json!({"mode":"compress","kind":"groth16","imageId":hex::encode(image_id()),
            "relation":statement.relation()? as u8,"journal":hex::encode(statement.abi_bytes()),
            "journalHash":hex::encode(statement.digest()),"seal":hex::encode(seal)})
        );
        return Ok(());
    }
    ensure!(
        command == "execute" || command == "prove",
        "commands are info, execute, prove, verify, compress"
    );
    let outputs = if command == "prove" {
        let (output, succinct) = prove_outputs(&args)?;
        let output = new_receipt_path(output)?;
        let succinct = succinct.map(new_receipt_path).transpose()?;
        ensure!(
            succinct.as_ref() != Some(&output),
            "succinct and Groth16 receipt paths must differ"
        );
        Some((output, succinct))
    } else {
        None
    };
    let input: ProofInput = serde_json::from_slice(&read_stdin(MAX_PRIVATE_INPUT)?)
        .map_err(|_| anyhow::anyhow!("invalid private proof input"))?;
    input.profile.validate()?;
    ensure!(
        input.profile.guest_image_id.as_slice() == image_id(),
        "profile must bind this actual guest image"
    );
    let mut builder = ExecutorEnv::builder();
    builder
        .segment_limit_po2(20)
        .session_limit(Some(input.profile.max_guest_cycles))
        .write(&input)?;
    let env = builder.build()?;
    let prover = local_prover()?;
    if command == "execute" {
        let session = prover.execute(env, VIBEFACE_DELIVERY_GUEST_ELF)?;
        ensure!(
            session.journal.bytes == input.statement.abi_bytes(),
            "execution journal mismatch"
        );
        let cycles: u64 = session
            .segments
            .iter()
            .map(|segment| u64::from(segment.cycles))
            .sum();
        ensure!(
            cycles <= input.profile.max_guest_cycles,
            "execution exceeds profile cycle cap"
        );
        println!(
            "{}",
            json!({"mode":"execute","imageId":hex::encode(image_id()),"profileId":hex::encode(input.profile.digest()?),
            "relation":input.statement.relation()? as u8,"journalHash":hex::encode(input.statement.digest()),"cycles":cycles,
            "segments":session.segments.len(),"proved":false})
        );
    } else {
        let (output, succinct) = outputs.context("missing proof output paths")?;
        let info =
            prover.prove_with_opts(env, VIBEFACE_DELIVERY_GUEST_ELF, &ProverOpts::succinct())?;
        checked_succinct_receipt(&info.receipt, input.statement.abi_bytes())?;
        if let Some(path) = succinct {
            let bytes = bincode::serialize(&info.receipt)?;
            ensure!(
                bytes.len() as u64 <= MAX_RECEIPT_INPUT,
                "succinct receipt exceeds local limit"
            );
            publish_receipt(&path, &bytes)?;
        }
        let receipt = prover.compress(&ProverOpts::groth16(), &info.receipt)?;
        checked_receipt(&receipt, input.statement.abi_bytes())?;
        let seal = groth16_seal(&receipt)?;
        publish_receipt(&output, &bincode::serialize(&receipt)?)?;
        println!(
            "{}",
            json!({"mode":"prove","kind":"groth16","imageId":hex::encode(image_id()),
            "profileId":hex::encode(input.profile.digest()?),"relation":input.statement.relation()? as u8,
            "journal":hex::encode(input.statement.abi_bytes()),"journalHash":hex::encode(input.statement.digest()),"seal":hex::encode(seal)})
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn fake_receipt() -> Receipt {
        Receipt::new(
            InnerReceipt::Fake(risc0_zkvm::FakeReceipt::new(risc0_zkvm::ReceiptClaim::ok(
                VIBEFACE_DELIVERY_GUEST_ID,
                Vec::<u8>::new(),
            ))),
            Vec::new(),
        )
    }

    #[test]
    fn checkpoint_is_an_explicit_optional_prove_argument() {
        let args = |values: &[&str]| {
            values
                .iter()
                .map(|value| value.to_string())
                .collect::<Vec<_>>()
        };
        let plain = args(&["prove", "final.receipt"]);
        assert_eq!(
            prove_outputs(&plain).unwrap(),
            (Path::new("final.receipt"), None)
        );
        let diagnostic = args(&[
            "prove",
            "final.receipt",
            "--succinct-receipt",
            "checkpoint.receipt",
        ]);
        assert_eq!(
            prove_outputs(&diagnostic).unwrap(),
            (
                Path::new("final.receipt"),
                Some(Path::new("checkpoint.receipt"))
            )
        );
        for invalid in [
            vec!["prove"],
            vec!["prove", "final.receipt", "--succinct-receipt"],
            vec!["prove", "final.receipt", "--other", "checkpoint.receipt"],
            vec![
                "prove",
                "final.receipt",
                "--succinct-receipt",
                "checkpoint.receipt",
                "extra",
            ],
        ] {
            assert!(prove_outputs(&args(&invalid)).is_err());
        }
    }

    #[test]
    fn receipt_input_is_bounded_strict_and_never_treats_fake_as_a_checkpoint() {
        let fake = fake_receipt();
        let mut bytes = bincode::serialize(&fake).unwrap();
        let decoded = decode_receipt(&bytes).unwrap();
        assert!(checked_receipt(&decoded, &[]).is_err());
        assert!(checked_succinct_receipt(&decoded, &[]).is_err());
        bytes.push(0);
        assert!(decode_receipt(&bytes).is_err());
        assert!(decode_receipt(&vec![0; MAX_RECEIPT_INPUT as usize + 1]).is_err());
        assert!(expected_statement("00").is_err());
        assert!(expected_statement(&"00".repeat(vibeface_delivery::STATEMENT_BYTES)).is_err());
    }

    #[test]
    fn completed_receipt_is_private_atomic_and_never_overwritten() {
        let base = std::env::var_os("CARGO_TARGET_DIR")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| {
                Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../.artifacts/proof-host-tests")
            });
        std::fs::create_dir_all(&base).unwrap();
        let directory = base.join(format!(
            "receipt-{}-{}",
            std::process::id(),
            RECEIPT_SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir(&directory).unwrap();
        let path = directory.join("statement.receipt");
        assert_eq!(
            new_receipt_path(&path).unwrap(),
            std::fs::canonicalize(&directory)
                .unwrap()
                .join("statement.receipt")
        );
        publish_receipt(&path, b"complete public receipt").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"complete public receipt");
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert!(publish_receipt(&path, b"replacement").is_err());
        assert!(new_receipt_path(&path).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), b"complete public receipt");
        assert_eq!(std::fs::read_dir(&directory).unwrap().count(), 1);
        let symlink = directory.join("dangling.receipt");
        std::os::unix::fs::symlink("missing.receipt", &symlink).unwrap();
        assert!(new_receipt_path(&symlink).is_err());
        assert!(publish_receipt(&symlink, b"replacement").is_err());
        std::fs::remove_dir_all(directory).unwrap();
    }
}
